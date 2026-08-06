//! A Flattened Device Tree (DTB) parser.
//!
//! Every real ARM64 bootloader hands the kernel one pointer — `x0` — and it
//! points at a device tree blob describing the machine: how much RAM there is
//! and where, which memory must *not* be touched, where the UART and the
//! interrupt controller live, how to start the other CPUs. Everything the
//! kernel currently hard-codes for QEMU `virt` is in here, and on a real device
//! guessing is not an option.
//!
//! Design constraints that shape the API:
//!
//! - **Zero allocation.** The blob is parsed before the heap exists — it is in
//!   fact one of the inputs to bringing the heap up. Nothing here allocates;
//!   everything borrows from the blob and iterates lazily.
//! - **No dependencies.** Same reason: this is the first thing to run.
//! - **Never panics on bad input.** The blob comes from firmware, which is
//!   outside our trust boundary. Every read is bounds-checked and every failure
//!   is an [`FdtError`], not a fault in early boot with no console.
//! - **Host-testable.** Pure logic over a byte slice, so the tests run against
//!   real DTBs dumped from QEMU on the host, exactly like `mm`.
//!
//! The format is big-endian regardless of CPU endianness, and is specified by
//! the Devicetree Specification. Structure:
//!
//! ```text
//! ┌──────────────┐ header (magic 0xd00dfeed, offsets, sizes)
//! ├──────────────┤ memory reservation block: (address, size) u64 pairs, (0,0) terminated
//! ├──────────────┤ structure block: a token stream (BEGIN_NODE/PROP/END_NODE/NOP/END)
//! └──────────────┘ strings block: NUL-terminated property names, referenced by offset
//! ```
//!
//! # Example
//!
//! ```no_run
//! # use staros_fdt::Fdt;
//! # fn demo(blob: &[u8]) -> Option<()> {
//! let fdt = Fdt::new(blob).ok()?;
//!
//! // Where is RAM?
//! for (base, size) in fdt.memory()? {
//!     // hand [base, base+size) to the frame allocator...
//! }
//!
//! // Where is the console?
//! let uart = fdt.find_compatible("arm,pl011")?;
//! let (base, _len) = uart.reg()?.next()?;
//! # Some(())
//! # }
//! ```
#![cfg_attr(not(test), no_std)]

use core::fmt;

/// The magic number at the start of every DTB, big-endian.
const FDT_MAGIC: u32 = 0xd00d_feed;

/// The structure-block format has been stable since version 16; version 17 adds
/// only the `size_dt_struct` header field. We accept any blob claiming
/// backwards compatibility with 16 (in practice everything emits 17).
const MIN_COMPATIBLE_VERSION: u32 = 16;

/// Header length in bytes for version 17.
const HEADER_LEN: usize = 40;

// Structure-block tokens.
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Maximum node nesting we track. Real device trees are shallow — QEMU `virt`
/// and Pixel trees are well under 10 — but the blob is untrusted input, so a
/// bound is required rather than assumed.
pub const MAX_DEPTH: usize = 32;

/// Devicetree-spec default when a node does not specify `#address-cells`.
const DEFAULT_ADDRESS_CELLS: u32 = 2;
/// Devicetree-spec default when a node does not specify `#size-cells`.
const DEFAULT_SIZE_CELLS: u32 = 1;

/// Why a blob could not be parsed.
///
/// Firmware is outside the trust boundary: a malformed blob must produce one of
/// these, never a panic or an out-of-bounds read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdtError {
    /// The header does not start with `0xd00dfeed` — not a DTB at all.
    BadMagic,
    /// The blob requires a reader newer than this one.
    UnsupportedVersion,
    /// A structure ran past the end of the blob.
    Truncated,
    /// The token stream is malformed (unknown token, unbalanced nodes, ...).
    BadStructure,
    /// A property name or node name is not valid UTF-8.
    BadString,
    /// Node nesting exceeded [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::BadMagic => "not a device tree blob (bad magic)",
            Self::UnsupportedVersion => "device tree blob version is too new",
            Self::Truncated => "device tree blob is truncated",
            Self::BadStructure => "device tree structure block is malformed",
            Self::BadString => "device tree contains a non-UTF-8 name",
            Self::TooDeep => "device tree nesting is too deep",
        };
        f.write_str(s)
    }
}

/// Read a big-endian `u32` at `offset`, or `None` if it would run off the end.
fn read_be_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Read a big-endian `u64` at `offset`, or `None` if it would run off the end.
fn read_be_u64(data: &[u8], offset: usize) -> Option<u64> {
    let hi = u64::from(read_be_u32(data, offset)?);
    let lo = u64::from(read_be_u32(data, offset.checked_add(4)?)?);
    Some((hi << 32) | lo)
}

/// Round `value` up to the next multiple of 4 (the token stream's alignment).
const fn align4(value: usize) -> Option<usize> {
    match value.checked_add(3) {
        Some(v) => Some(v & !3),
        None => None,
    }
}

/// Read `n` big-endian cells starting at `offset` and assemble them into a
/// `u64`, most-significant cell first. `n` above 2 keeps only the low 64 bits,
/// which matches how the kernel uses addresses.
fn read_cells(data: &[u8], offset: usize, n: u32) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..n as usize {
        let cell = read_be_u32(data, offset.checked_add(i.checked_mul(4)?)?)?;
        value = (value << 32) | u64::from(cell);
    }
    Some(value)
}

/// The pieces of the blob every borrowed view needs. `Copy` so nodes and
/// iterators can carry it around freely.
#[derive(Clone, Copy)]
struct Blob<'a> {
    /// The whole blob, already bounds-checked to `totalsize`.
    data: &'a [u8],
    /// Byte range of the structure block within `data`.
    struct_start: usize,
    struct_end: usize,
    /// Byte range of the strings block within `data`.
    strings_start: usize,
    strings_end: usize,
}

impl<'a> Blob<'a> {
    /// Resolve a property name by its offset into the strings block.
    fn string_at(&self, offset: usize) -> Result<&'a str, FdtError> {
        let start = self.strings_start.checked_add(offset).ok_or(FdtError::Truncated)?;
        let region = self
            .data
            .get(start..self.strings_end)
            .ok_or(FdtError::Truncated)?;
        let len = region.iter().position(|&b| b == 0).ok_or(FdtError::BadStructure)?;
        core::str::from_utf8(&region[..len]).map_err(|_| FdtError::BadString)
    }
}

/// A parsed device tree blob.
///
/// Holds no owned data — it borrows the blob for its whole lifetime, so the
/// caller must keep the firmware-provided memory alive (and out of the frame
/// allocator's hands).
#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    blob: Blob<'a>,
    mem_rsvmap: usize,
    boot_cpuid: u32,
}

impl<'a> Fdt<'a> {
    /// Parse the header of `data` and validate every offset it declares.
    ///
    /// After this returns `Ok`, the structure and strings blocks are known to be
    /// inside the slice — which is what lets the rest of the parser treat its
    /// own bounds checks as belt-and-braces rather than the only line of
    /// defence.
    ///
    /// # Errors
    /// [`FdtError::BadMagic`] if this is not a DTB, [`FdtError::UnsupportedVersion`]
    /// if it is too new to read, [`FdtError::Truncated`] if the slice is shorter
    /// than the header says or any declared block falls outside it.
    pub fn new(data: &'a [u8]) -> Result<Self, FdtError> {
        if read_be_u32(data, 0).ok_or(FdtError::Truncated)? != FDT_MAGIC {
            return Err(FdtError::BadMagic);
        }
        let totalsize = read_be_u32(data, 4).ok_or(FdtError::Truncated)? as usize;
        if totalsize < HEADER_LEN || totalsize > data.len() {
            return Err(FdtError::Truncated);
        }
        // Ignore any trailing padding: firmware routinely reserves extra space
        // (QEMU pads the blob to 1 MiB) and `totalsize` is the authority.
        let data = &data[..totalsize];

        let off_dt_struct = read_be_u32(data, 8).ok_or(FdtError::Truncated)? as usize;
        let off_dt_strings = read_be_u32(data, 12).ok_or(FdtError::Truncated)? as usize;
        let off_mem_rsvmap = read_be_u32(data, 16).ok_or(FdtError::Truncated)? as usize;
        let last_comp_version = read_be_u32(data, 24).ok_or(FdtError::Truncated)?;
        let boot_cpuid = read_be_u32(data, 28).ok_or(FdtError::Truncated)?;
        let size_dt_strings = read_be_u32(data, 32).ok_or(FdtError::Truncated)? as usize;
        let size_dt_struct = read_be_u32(data, 36).ok_or(FdtError::Truncated)? as usize;

        if last_comp_version > MIN_COMPATIBLE_VERSION {
            return Err(FdtError::UnsupportedVersion);
        }

        let struct_end = off_dt_struct.checked_add(size_dt_struct).ok_or(FdtError::Truncated)?;
        let strings_end = off_dt_strings.checked_add(size_dt_strings).ok_or(FdtError::Truncated)?;
        if struct_end > totalsize || strings_end > totalsize || off_mem_rsvmap >= totalsize {
            return Err(FdtError::Truncated);
        }

        Ok(Self {
            blob: Blob {
                data,
                struct_start: off_dt_struct,
                struct_end,
                strings_start: off_dt_strings,
                strings_end,
            },
            mem_rsvmap: off_mem_rsvmap,
            boot_cpuid,
        })
    }

    /// Parse a blob the bootloader left in memory, given only its base pointer.
    ///
    /// This is the `x0` path: read the header to learn `totalsize`, then build
    /// the slice. Two-step, because the length is inside the data.
    ///
    /// # Safety
    /// If `ptr` is non-null and 8-byte aligned it must point at readable memory
    /// of at least [`HEADER_LEN`] bytes, and if that memory declares a valid DTB
    /// then `totalsize` bytes from `ptr` must be mapped and stay unmodified for
    /// `'a`. In practice this means the caller has already excluded the blob from
    /// the frame allocator. Null and misaligned pointers are *rejected*, not
    /// dereferenced, so passing a raw `x0` straight from firmware is fine.
    ///
    /// # Errors
    /// [`FdtError::BadMagic`] for a null, misaligned, or non-DTB pointer;
    /// otherwise as [`Fdt::new`].
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Self, FdtError> {
        // Check before dereferencing, not after. `x0` is whatever the bootloader
        // felt like leaving there — on a machine that passes no device tree it is
        // simply zero — and `slice::from_raw_parts` requires non-null and aligned
        // as a *precondition*, so validating inside the slice would be too late.
        // The spec requires the blob to be 8-byte aligned.
        if ptr.is_null() || !(ptr as usize).is_multiple_of(8) {
            return Err(FdtError::BadMagic);
        }
        // SAFETY: non-null and aligned per the check above; the caller guarantees
        // the header is mapped and readable. We read exactly the 8 bytes needed
        // to learn the real length before touching anything else.
        let header = unsafe { core::slice::from_raw_parts(ptr, 8) };
        if read_be_u32(header, 0).ok_or(FdtError::Truncated)? != FDT_MAGIC {
            return Err(FdtError::BadMagic);
        }
        let totalsize = read_be_u32(header, 4).ok_or(FdtError::Truncated)? as usize;
        if totalsize < HEADER_LEN {
            return Err(FdtError::Truncated);
        }
        // SAFETY: the caller guarantees `totalsize` bytes from `ptr` are mapped
        // and immutable for `'a`; `totalsize` is what the blob itself declares.
        let data = unsafe { core::slice::from_raw_parts(ptr, totalsize) };
        Self::new(data)
    }

    /// Total blob length in bytes, as declared by the header. The region
    /// `[ptr, ptr + total_size())` is what the kernel must keep reserved.
    #[must_use]
    pub fn total_size(&self) -> usize {
        self.blob.data.len()
    }

    /// The physical CPU id of the core that is running the boot code.
    #[must_use]
    pub fn boot_cpuid(&self) -> u32 {
        self.boot_cpuid
    }

    /// The memory reservation block: regions the firmware forbids the kernel to
    /// use, listed in the header rather than the tree.
    ///
    /// Note this is *not* the same as `/reserved-memory`, which is a node and
    /// carries the bulk of the carveouts on a real device. A kernel must honour
    /// both — see [`Fdt::reserved_memory`].
    #[must_use]
    pub fn memory_reservations(&self) -> MemoryReservations<'a> {
        MemoryReservations { data: self.blob.data, pos: self.mem_rsvmap }
    }

    /// The root node, `/`.
    ///
    /// # Errors
    /// [`FdtError::BadStructure`] if the structure block does not begin with a
    /// node, as the spec requires.
    pub fn root(&self) -> Result<Node<'a>, FdtError> {
        let mut walk = Walk::new(self.blob);
        walk.next_node()?.ok_or(FdtError::BadStructure)
    }

    /// Every node in the tree, in depth-first order.
    #[must_use]
    pub fn nodes(&self) -> Nodes<'a> {
        Nodes { walk: Walk::new(self.blob) }
    }

    /// Find a node by absolute path, e.g. `/soc/serial@7e201000`.
    ///
    /// A path component without a unit address matches a node that has one:
    /// `/memory` finds `/memory@40000000`. This matters because those addresses
    /// vary per board, so hard-coding the full name would defeat the purpose.
    #[must_use]
    pub fn find_node(&self, path: &str) -> Option<Node<'a>> {
        let mut walk = Walk::new(self.blob);
        loop {
            let node = walk.next_node().ok()??;
            if walk.matches_path(path) {
                return Some(node);
            }
        }
    }

    /// Find the first node whose `compatible` list contains `compatible`.
    ///
    /// This is how a driver is bound to hardware without knowing the board: ask
    /// for `arm,pl011` rather than for address `0x0900_0000`.
    #[must_use]
    pub fn find_compatible(&self, compatible: &str) -> Option<Node<'a>> {
        self.nodes().find(|node| node.is_compatible(compatible))
    }

    /// All nodes whose `compatible` list contains `compatible`. A machine can
    /// have several of a thing (four UARTs, many virtio slots).
    pub fn find_all_compatible(&self, compatible: &'a str) -> impl Iterator<Item = Node<'a>> {
        self.nodes().filter(move |node| node.is_compatible(compatible))
    }

    /// The usable RAM banks as `(base, size)` pairs, from the `/memory` node.
    ///
    /// Returns `None` if there is no `/memory` node or it has no `reg` — on a
    /// real boot that is fatal and the kernel has nothing to allocate from.
    /// **The regions are raw:** the kernel must still subtract its own image,
    /// the blob, the initrd, [`Fdt::memory_reservations`] and
    /// [`Fdt::reserved_memory`] before handing anything to the allocator.
    #[must_use]
    pub fn memory(&self) -> Option<Reg<'a>> {
        self.find_node("/memory")?.reg()
    }

    /// The `/reserved-memory` carveouts as `(base, size)` pairs.
    ///
    /// On QEMU this is usually empty; on a phone it is a long list (trusted
    /// firmware, modem, framebuffer, ...) and writing into any of it is an
    /// instant silent reset. Child nodes without a `reg` (dynamic allocations
    /// requested via `size`/`alignment`) are skipped — nothing is reserved yet,
    /// so there is nothing to avoid.
    pub fn reserved_memory(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.find_node("/reserved-memory")
            .into_iter()
            .flat_map(|node| node.children())
            .filter_map(|child| child.reg())
            .flatten()
    }

    /// The `bootargs` string from `/chosen`, i.e. the kernel command line.
    #[must_use]
    pub fn bootargs(&self) -> Option<&'a str> {
        self.find_node("/chosen")?.property("bootargs")?.as_str()
    }

    /// The initrd/initramfs image the bootloader loaded, as `(start, end)`
    /// physical addresses from `/chosen`.
    ///
    /// This is how the first user-space image arrives on a real device — the
    /// bootloader already placed it in RAM, so no storage driver is needed to
    /// get a userland running.
    #[must_use]
    pub fn initrd(&self) -> Option<(u64, u64)> {
        let chosen = self.find_node("/chosen")?;
        // The properties are 32-bit on some trees and 64-bit on others; accept
        // either rather than demanding a width the board didn't choose.
        let start = chosen.property("linux,initrd-start")?.as_uint()?;
        let end = chosen.property("linux,initrd-end")?.as_uint()?;
        Some((start, end))
    }
}

impl fmt::Debug for Fdt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fdt")
            .field("total_size", &self.blob.data.len())
            .field("boot_cpuid", &self.boot_cpuid)
            .finish()
    }
}

/// Iterator over the header's memory reservation block.
#[derive(Clone)]
pub struct MemoryReservations<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Iterator for MemoryReservations<'_> {
    /// `(address, size)` of a reserved region.
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        let address = read_be_u64(self.data, self.pos)?;
        let size = read_be_u64(self.data, self.pos + 8)?;
        // The block is terminated by an all-zero entry.
        if address == 0 && size == 0 {
            return None;
        }
        self.pos += 16;
        Some((address, size))
    }
}

/// A cursor over the structure block that tracks node nesting.
///
/// Both [`Fdt::nodes`] and [`Fdt::find_node`] are built on this. It walks tokens
/// linearly — no recursion, which keeps the early-boot stack bounded — and
/// maintains the enclosing node names and cell counts as it goes. Because the
/// spec puts a node's properties before any of its children, a node's
/// `#address-cells` is always known by the time we reach the children it
/// applies to.
struct Walk<'a> {
    blob: Blob<'a>,
    pos: usize,
    /// Number of nodes currently open.
    depth: usize,
    /// Name of the node open at each level.
    names: [&'a str; MAX_DEPTH],
    /// `(#address-cells, #size-cells)` in force for the children of the node
    /// open at each level. Inherited from the parent until overridden, which is
    /// what Linux does and what real trees rely on.
    cells: [(u32, u32); MAX_DEPTH],
    /// Set once the token stream has ended.
    done: bool,
}

impl<'a> Walk<'a> {
    fn new(blob: Blob<'a>) -> Self {
        Self {
            blob,
            pos: blob.struct_start,
            depth: 0,
            names: [""; MAX_DEPTH],
            cells: [(DEFAULT_ADDRESS_CELLS, DEFAULT_SIZE_CELLS); MAX_DEPTH],
            done: false,
        }
    }

    /// Advance to the next `FDT_BEGIN_NODE` and return the node it opens,
    /// consuming (and accounting for) the properties and end-tokens on the way.
    fn next_node(&mut self) -> Result<Option<Node<'a>>, FdtError> {
        loop {
            if self.done || self.pos >= self.blob.struct_end {
                return Ok(None);
            }
            let token = read_be_u32(self.blob.data, self.pos).ok_or(FdtError::Truncated)?;
            self.pos += 4;
            match token {
                FDT_NOP => {}
                FDT_END => {
                    self.done = true;
                    return Ok(None);
                }
                FDT_END_NODE => {
                    self.depth = self.depth.checked_sub(1).ok_or(FdtError::BadStructure)?;
                }
                FDT_BEGIN_NODE => {
                    let name = self.read_name()?;
                    if self.depth >= MAX_DEPTH {
                        return Err(FdtError::TooDeep);
                    }
                    // Cells in force for this node's *children* start as the
                    // parent's and may be overridden by this node's own props.
                    let inherited = if self.depth == 0 {
                        (DEFAULT_ADDRESS_CELLS, DEFAULT_SIZE_CELLS)
                    } else {
                        self.cells[self.depth - 1]
                    };
                    self.names[self.depth] = name;
                    self.cells[self.depth] = inherited;
                    self.depth += 1;
                    return Ok(Some(Node {
                        blob: self.blob,
                        name,
                        body: self.pos,
                        parent_cells: inherited,
                    }));
                }
                FDT_PROP => {
                    let (name, value) = self.read_prop()?;
                    // Record cell counts for the node currently open, so its
                    // children see them.
                    let level = self.depth.checked_sub(1).ok_or(FdtError::BadStructure)?;
                    if let Some(n) = read_be_u32(value, 0) {
                        match name {
                            "#address-cells" => self.cells[level].0 = n,
                            "#size-cells" => self.cells[level].1 = n,
                            _ => {}
                        }
                    }
                }
                _ => return Err(FdtError::BadStructure),
            }
        }
    }

    /// Read the NUL-terminated node name following an `FDT_BEGIN_NODE`.
    fn read_name(&mut self) -> Result<&'a str, FdtError> {
        let region = self
            .blob
            .data
            .get(self.pos..self.blob.struct_end)
            .ok_or(FdtError::Truncated)?;
        let len = region.iter().position(|&b| b == 0).ok_or(FdtError::BadStructure)?;
        let name = core::str::from_utf8(&region[..len]).map_err(|_| FdtError::BadString)?;
        self.pos = align4(self.pos.checked_add(len + 1).ok_or(FdtError::Truncated)?)
            .ok_or(FdtError::Truncated)?;
        Ok(name)
    }

    /// Read the length/name-offset/value following an `FDT_PROP`.
    fn read_prop(&mut self) -> Result<(&'a str, &'a [u8]), FdtError> {
        let len = read_be_u32(self.blob.data, self.pos).ok_or(FdtError::Truncated)? as usize;
        let nameoff = read_be_u32(self.blob.data, self.pos + 4).ok_or(FdtError::Truncated)? as usize;
        let value_start = self.pos + 8;
        let value_end = value_start.checked_add(len).ok_or(FdtError::Truncated)?;
        let value = self
            .blob
            .data
            .get(value_start..value_end)
            .ok_or(FdtError::Truncated)?;
        let name = self.blob.string_at(nameoff)?;
        self.pos = align4(value_end).ok_or(FdtError::Truncated)?;
        Ok((name, value))
    }

    /// Does the currently open node sit at exactly `path`?
    ///
    /// Compares the open-node stack against the path's components, so it costs
    /// nothing to maintain and needs no string building.
    fn matches_path(&self, path: &str) -> bool {
        // The root's own name is empty; components are everything after it.
        let mut components = path.split('/').filter(|c| !c.is_empty());
        let mut level = 1;
        for component in &mut components {
            if level >= self.depth || !name_matches(self.names[level], component) {
                return false;
            }
            level += 1;
        }
        level == self.depth
    }
}

/// Does node name `name` satisfy path component `component`?
///
/// Exact match, or `component` names the node and lets the unit address float:
/// `memory` matches `memory@40000000`. The addresses are board-specific, so
/// requiring them would defeat the point of reading the tree at all.
fn name_matches(name: &str, component: &str) -> bool {
    if name == component {
        return true;
    }
    match name.split_once('@') {
        Some((base, _unit)) => base == component && !component.contains('@'),
        None => false,
    }
}

/// Iterator over every node in the tree, depth-first.
pub struct Nodes<'a> {
    walk: Walk<'a>,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        // A malformed blob ends iteration rather than propagating; callers that
        // need to distinguish "absent" from "corrupt" validate with `Fdt::new`
        // and the typed accessors.
        self.walk.next_node().ok().flatten()
    }
}

/// A node in the device tree.
///
/// Borrows the blob; carries the offset of its first token and the cell counts
/// its parent imposed (which is what makes [`Node::reg`] work without a second
/// lookup).
#[derive(Clone, Copy)]
pub struct Node<'a> {
    blob: Blob<'a>,
    name: &'a str,
    /// Offset of the first token inside this node.
    body: usize,
    /// `(#address-cells, #size-cells)` this node's parent specified.
    parent_cells: (u32, u32),
}

impl<'a> Node<'a> {
    /// The node's name as it appears in the tree, e.g. `serial@7e201000`. The
    /// root's name is the empty string.
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The name with any `@unit-address` stripped: `serial@7e201000` → `serial`.
    #[must_use]
    pub fn base_name(&self) -> &'a str {
        match self.name.split_once('@') {
            Some((base, _)) => base,
            None => self.name,
        }
    }

    /// The `@unit-address` part of the name, if any, parsed as hex.
    ///
    /// Convenient but not authoritative — the unit address is a naming
    /// convention. `reg` is the truth.
    #[must_use]
    pub fn unit_address(&self) -> Option<u64> {
        let (_, unit) = self.name.split_once('@')?;
        u64::from_str_radix(unit, 16).ok()
    }

    /// The node's own properties, in tree order.
    #[must_use]
    pub fn properties(&self) -> Properties<'a> {
        Properties { blob: self.blob, pos: self.body, done: false }
    }

    /// Look up one property by name.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<Property<'a>> {
        self.properties().find(|p| p.name == name)
    }

    /// The node's immediate children.
    #[must_use]
    pub fn children(&self) -> Children<'a> {
        Children { blob: self.blob, pos: self.body, cells: self.cells(), done: false }
    }

    /// `#address-cells` for this node's children: its own value, else inherited
    /// from the parent.
    #[must_use]
    pub fn address_cells(&self) -> u32 {
        self.property("#address-cells")
            .and_then(|p| p.as_u32())
            .unwrap_or(self.parent_cells.0)
    }

    /// `#size-cells` for this node's children: its own value, else inherited.
    #[must_use]
    pub fn size_cells(&self) -> u32 {
        self.property("#size-cells")
            .and_then(|p| p.as_u32())
            .unwrap_or(self.parent_cells.1)
    }

    /// This node's `(#address-cells, #size-cells)` as seen by its children.
    fn cells(&self) -> (u32, u32) {
        (self.address_cells(), self.size_cells())
    }

    /// Does this node's `compatible` list contain `compatible`?
    ///
    /// `compatible` is a list from most to least specific (`"arm,gic-v3"` then
    /// `"arm,cortex-a72"` style), and a match on any entry binds.
    #[must_use]
    pub fn is_compatible(&self, compatible: &str) -> bool {
        self.property("compatible")
            .is_some_and(|p| p.strings().any(|s| s == compatible))
    }

    /// The node's `reg` decoded as `(address, size)` pairs, using the cell
    /// counts its **parent** declared — which is why cells are tracked during
    /// the walk rather than read on demand.
    ///
    /// Returns `None` if the node has no `reg`.
    ///
    /// Caveat worth knowing before trusting the address on a real board: `reg`
    /// is expressed in the parent bus's address space. Under a bus with
    /// `ranges`, translating to a CPU physical address means applying those
    /// ranges. QEMU `virt` and most phone `soc` nodes use empty (identity)
    /// `ranges`, so this is the physical address there; a non-identity bus is a
    /// case to handle when a board first needs it.
    #[must_use]
    pub fn reg(&self) -> Option<Reg<'a>> {
        let property = self.property("reg")?;
        Some(Reg {
            data: property.value,
            pos: 0,
            address_cells: self.parent_cells.0,
            size_cells: self.parent_cells.1,
        })
    }

    /// This node's `interrupts`, as raw cell groups.
    ///
    /// `cells` is the interrupt controller's `#interrupt-cells` — 3 on every ARM
    /// machine. The caller supplies it rather than the parser chasing
    /// `interrupt-parent` phandles, because the caller is the thing that knows
    /// what the cells mean.
    #[must_use]
    pub fn interrupts(&self, cells: u32) -> Option<Interrupts<'a>> {
        let prop = self.property("interrupts")?;
        Some(Interrupts {
            data: prop.raw(),
            pos: 0,
            cells,
        })
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node").field("name", &self.name).finish()
    }
}

/// Iterator over a node's own properties. Stops at the first child node.
pub struct Properties<'a> {
    blob: Blob<'a>,
    pos: usize,
    done: bool,
}

impl<'a> Iterator for Properties<'a> {
    type Item = Property<'a>;

    fn next(&mut self) -> Option<Property<'a>> {
        loop {
            if self.done {
                return None;
            }
            let token = read_be_u32(self.blob.data, self.pos)?;
            match token {
                FDT_NOP => self.pos += 4,
                FDT_PROP => {
                    let len = read_be_u32(self.blob.data, self.pos + 4)? as usize;
                    let nameoff = read_be_u32(self.blob.data, self.pos + 8)? as usize;
                    let value_start = self.pos + 12;
                    let value = self.blob.data.get(value_start..value_start.checked_add(len)?)?;
                    let name = self.blob.string_at(nameoff).ok()?;
                    self.pos = align4(value_start + len)?;
                    return Some(Property { name, value });
                }
                // A child node or the end of this one: no properties left.
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// Iterator over a node's immediate children.
pub struct Children<'a> {
    blob: Blob<'a>,
    pos: usize,
    /// The parent's cells, handed to each child so its `reg` decodes correctly.
    cells: (u32, u32),
    done: bool,
}

impl<'a> Children<'a> {
    /// Skip the subtree of a node whose `FDT_BEGIN_NODE` has been consumed,
    /// leaving `pos` just past its matching `FDT_END_NODE`.
    fn skip_subtree(&mut self) -> Option<()> {
        let mut depth = 1usize;
        while depth > 0 {
            let token = read_be_u32(self.blob.data, self.pos)?;
            self.pos += 4;
            match token {
                FDT_NOP => {}
                FDT_BEGIN_NODE => {
                    depth += 1;
                    self.pos = align4(self.pos + skip_cstr(self.blob.data, self.pos)?)?;
                }
                FDT_END_NODE => depth -= 1,
                FDT_PROP => {
                    let len = read_be_u32(self.blob.data, self.pos)? as usize;
                    self.pos = align4(self.pos + 8 + len)?;
                }
                _ => return None,
            }
        }
        Some(())
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        loop {
            if self.done {
                return None;
            }
            let token = read_be_u32(self.blob.data, self.pos)?;
            self.pos += 4;
            match token {
                FDT_NOP => {}
                // Properties of the parent, emitted before any child.
                FDT_PROP => {
                    let len = read_be_u32(self.blob.data, self.pos)? as usize;
                    self.pos = align4(self.pos + 8 + len)?;
                }
                FDT_BEGIN_NODE => {
                    let len = skip_cstr(self.blob.data, self.pos)?;
                    let name = core::str::from_utf8(self.blob.data.get(self.pos..self.pos + len - 1)?)
                        .ok()?;
                    self.pos = align4(self.pos + len)?;
                    let node = Node {
                        blob: self.blob,
                        name,
                        body: self.pos,
                        parent_cells: self.cells,
                    };
                    self.skip_subtree()?;
                    return Some(node);
                }
                // The parent's own END_NODE: no children left.
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// Length of the NUL-terminated string at `pos`, including the terminator.
fn skip_cstr(data: &[u8], pos: usize) -> Option<usize> {
    let region = data.get(pos..)?;
    Some(region.iter().position(|&b| b == 0)? + 1)
}

/// A property: a name and an opaque byte value whose meaning is per-binding.
///
/// The accessors below are the standard encodings; none of them panic on a
/// value of the wrong length, they return `None`.
#[derive(Clone, Copy)]
pub struct Property<'a> {
    name: &'a str,
    value: &'a [u8],
}

impl<'a> Property<'a> {
    /// The property's name.
    #[must_use]
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The raw value bytes, for bindings the accessors don't cover.
    #[must_use]
    pub fn raw(&self) -> &'a [u8] {
        self.value
    }

    /// A property with no value, used as a flag (`interrupt-controller`,
    /// `dma-coherent`, `ranges`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// The value as a single big-endian `u32` cell.
    #[must_use]
    pub fn as_u32(&self) -> Option<u32> {
        if self.value.len() != 4 {
            return None;
        }
        read_be_u32(self.value, 0)
    }

    /// The value as a single big-endian `u64` (two cells).
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        if self.value.len() != 8 {
            return None;
        }
        read_be_u64(self.value, 0)
    }

    /// The value as an unsigned integer of *either* width.
    ///
    /// Some bindings (`linux,initrd-start` is the classic) are 32-bit on one
    /// board and 64-bit on the next; accepting both beats demanding a width the
    /// firmware didn't pick.
    #[must_use]
    pub fn as_uint(&self) -> Option<u64> {
        match self.value.len() {
            4 => self.as_u32().map(u64::from),
            8 => self.as_u64(),
            _ => None,
        }
    }

    /// The value as a NUL-terminated string.
    #[must_use]
    pub fn as_str(&self) -> Option<&'a str> {
        let end = self.value.iter().position(|&b| b == 0).unwrap_or(self.value.len());
        core::str::from_utf8(&self.value[..end]).ok()
    }

    /// The value as a list of NUL-terminated strings (`compatible`,
    /// `clock-names`).
    #[must_use]
    pub fn strings(&self) -> Strings<'a> {
        Strings { data: self.value }
    }

    /// The value as a sequence of big-endian `u32` cells.
    #[must_use]
    pub fn cells(&self) -> Cells<'a> {
        Cells { data: self.value, pos: 0 }
    }
}

impl fmt::Debug for Property<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Property")
            .field("name", &self.name)
            .field("len", &self.value.len())
            .finish()
    }
}

/// Iterator over a string-list property.
#[derive(Clone)]
pub struct Strings<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Strings<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.data.is_empty() {
            return None;
        }
        let end = self.data.iter().position(|&b| b == 0).unwrap_or(self.data.len());
        let (head, tail) = self.data.split_at(end);
        // Step over the NUL, unless the last string was unterminated.
        self.data = tail.get(1..).unwrap_or(&[]);
        core::str::from_utf8(head).ok()
    }
}

/// Iterator over a property's raw `u32` cells.
#[derive(Clone)]
pub struct Cells<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Iterator for Cells<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let cell = read_be_u32(self.data, self.pos)?;
        self.pos += 4;
        Some(cell)
    }
}

/// Iterator over a `reg` property, decoded into `(address, size)` pairs
/// according to the parent's cell counts.
#[derive(Clone)]
pub struct Reg<'a> {
    data: &'a [u8],
    pos: usize,
    address_cells: u32,
    size_cells: u32,
}

impl Iterator for Reg<'_> {
    /// `(address, size)`. A `#size-cells` of 0 (as on `/cpus`) yields size 0.
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        let address = read_cells(self.data, self.pos, self.address_cells)?;
        let size_at = self.pos + self.address_cells as usize * 4;
        let size = read_cells(self.data, size_at, self.size_cells)?;
        let entry = (self.address_cells as usize + self.size_cells as usize) * 4;
        if entry == 0 {
            return None;
        }
        self.pos += entry;
        Some((address, size))
    }
}

/// The `interrupts` property of a node, as a sequence of raw cell groups.
///
/// The *meaning* of the cells belongs to whichever interrupt controller the node
/// hangs off — this iterator only knows how many cells make one entry, which is
/// what `#interrupt-cells` on the controller says. For every ARM machine that is
/// 3: `(kind, number, flags)`, and turning that into an interrupt id is the GIC's
/// business, not the parser's.
pub struct Interrupts<'a> {
    data: &'a [u8],
    pos: usize,
    cells: u32,
}

impl Iterator for Interrupts<'_> {
    /// One entry, as up to three raw cells. Fewer than three are reported as 0.
    type Item = (u32, u32, u32);

    fn next(&mut self) -> Option<(u32, u32, u32)> {
        if self.cells == 0 {
            return None;
        }
        let cell = |i: u32| -> Option<u32> {
            if i >= self.cells {
                return Some(0);
            }
            read_cells(self.data, self.pos + i as usize * 4, 1).map(|v| v as u32)
        };
        let entry = (cell(0)?, cell(1)?, cell(2)?);
        self.pos += self.cells as usize * 4;
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real device trees dumped from QEMU (`-machine dumpdtb=...`). Testing
    /// against firmware output rather than hand-built blobs is the whole point:
    /// the bugs live in what real emitters actually produce.
    const VIRT_GICV2: &[u8] = include_bytes!("../tests/data/virt-gicv2.dtb");
    const VIRT_GICV3: &[u8] = include_bytes!("../tests/data/virt-gicv3.dtb");

    fn gicv3() -> Fdt<'static> {
        Fdt::new(VIRT_GICV3).expect("virt-gicv3.dtb should parse")
    }

    /// Copy `blob` into 8-byte-aligned storage, as firmware leaves it in RAM.
    fn aligned_copy(blob: &[u8]) -> Vec<u64> {
        let mut words = vec![0u64; blob.len().div_ceil(8)];
        // SAFETY: `words` owns at least `blob.len()` bytes, and `u64` has no
        // invalid bit patterns, so viewing it as bytes to fill is sound.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), blob.len())
        };
        bytes.copy_from_slice(blob);
        words
    }

    #[test]
    fn parses_real_qemu_blobs() {
        for blob in [VIRT_GICV2, VIRT_GICV3] {
            let fdt = Fdt::new(blob).expect("real DTB should parse");
            assert_eq!(fdt.total_size(), blob.len());
            assert_eq!(fdt.root().unwrap().name(), "");
        }
    }

    #[test]
    fn rejects_malformed_blobs_without_panicking() {
        assert_eq!(Fdt::new(&[]).unwrap_err(), FdtError::Truncated);
        assert_eq!(Fdt::new(&[0; 64]).unwrap_err(), FdtError::BadMagic);

        // A valid header whose declared totalsize exceeds the slice.
        let mut short = VIRT_GICV3[..HEADER_LEN].to_vec();
        assert_eq!(Fdt::new(&short).unwrap_err(), FdtError::Truncated);

        // Truncating a real blob must be caught, never read out of bounds.
        short = VIRT_GICV3[..VIRT_GICV3.len() / 2].to_vec();
        assert_eq!(Fdt::new(&short).unwrap_err(), FdtError::Truncated);

        // Every single-byte corruption either parses or errors — but a full
        // walk of the tree must never panic on any of them.
        for i in (0..VIRT_GICV3.len()).step_by(7) {
            let mut fuzzed = VIRT_GICV3.to_vec();
            fuzzed[i] ^= 0xff;
            if let Ok(fdt) = Fdt::new(&fuzzed) {
                for node in fdt.nodes() {
                    for property in node.properties() {
                        let _ = property.as_uint();
                        let _ = property.strings().count();
                    }
                    let _ = node.reg().map(Iterator::count);
                }
            }
        }
    }

    #[test]
    fn finds_memory_with_root_cell_counts() {
        let fdt = gicv3();
        // The root declares #address-cells = <2> and #size-cells = <2>, so a
        // parser that assumed the spec defaults (2/1) would decode this wrong.
        let root = fdt.root().unwrap();
        assert_eq!(root.address_cells(), 2);
        assert_eq!(root.size_cells(), 2);

        let banks: Vec<_> = fdt.memory().expect("virt has /memory").collect();
        // Launched with -m 2G at the virt machine's RAM base.
        assert_eq!(banks, vec![(0x4000_0000, 0x8000_0000)]);
    }

    #[test]
    fn path_lookup_tolerates_unit_addresses() {
        let fdt = gicv3();
        // The unit address is board-specific; looking up by bare name must work.
        assert_eq!(fdt.find_node("/memory").unwrap().name(), "memory@40000000");
        assert_eq!(fdt.find_node("/memory@40000000").unwrap().name(), "memory@40000000");
        assert_eq!(fdt.find_node("/chosen").unwrap().name(), "chosen");
        assert_eq!(fdt.find_node("/cpus/cpu@0").unwrap().base_name(), "cpu");
        assert_eq!(fdt.find_node("/cpus/cpu").unwrap().unit_address(), Some(0));

        // Non-existent paths, and prefixes that must not match deeper nodes.
        assert!(fdt.find_node("/nope").is_none());
        assert!(fdt.find_node("/cpus/cpu@0/deeper").is_none());
        assert!(fdt.find_node("/memor").is_none());
    }

    #[test]
    fn binds_drivers_by_compatible_not_by_address() {
        let fdt = gicv3();

        // This is the point of the whole crate: the UART's address comes from
        // the tree, not from a constant. On virt it happens to be the value
        // `arch-aarch64::uart::PL011_BASE` hard-codes today.
        let uart = fdt.find_compatible("arm,pl011").expect("virt has a PL011");
        let (base, len) = uart.reg().unwrap().next().unwrap();
        assert_eq!(base, 0x0900_0000);
        assert_eq!(len, 0x1000);

        // A GICv3 has two reg ranges: distributor and redistributors. Getting
        // both out is exactly what the GICv3 driver will need.
        let gic = fdt.find_compatible("arm,gic-v3").expect("gic-version=3");
        let ranges: Vec<_> = gic.reg().unwrap().collect();
        assert_eq!(ranges.len(), 2, "GICD + GICR");
        assert_eq!(ranges[0], (0x0800_0000, 0x1_0000));

        // The same query against the GICv2 blob finds nothing, and the v2
        // controller instead — which is how one kernel image will pick a driver
        // at runtime.
        let v2 = Fdt::new(VIRT_GICV2).unwrap();
        assert!(v2.find_compatible("arm,gic-v3").is_none());
        assert!(v2.find_compatible("arm,cortex-a15-gic").is_some());
    }

    #[test]
    fn decodes_the_interrupts_the_kernel_hard_coded() {
        // Both of these were constants in the kernel — 33 for the UART, 30 for
        // the timer — and both are right there in the tree. The GIC binding is
        // `<kind, number, flags>`: kind 0 = SPI (id = 32 + number), kind 1 = PPI
        // (id = 16 + number).
        let fdt = gicv3();

        let uart = fdt.find_compatible("arm,pl011").expect("virt has a pl011");
        let (kind, number, _flags) = uart.interrupts(3).unwrap().next().unwrap();
        assert_eq!((kind, number), (0, 1), "the UART is SPI 1");
        assert_eq!(32 + number, 33, "which is the INTID the kernel used to hard-code");

        // The timer declares four lines; the second is the non-secure EL1
        // physical timer, which is the one this kernel arms.
        let timer = fdt.find_compatible("arm,armv8-timer").expect("virt has a timer");
        let lines: Vec<(u32, u32, u32)> = timer.interrupts(3).unwrap().collect();
        assert_eq!(lines.len(), 4, "secure phys, non-secure phys, virtual, hyp");
        let (kind, number, _) = lines[1];
        assert_eq!((kind, number), (1, 14), "non-secure physical timer is PPI 14");
        assert_eq!(16 + number, 30, "which is TIMER_INTID");
    }

    #[test]
    fn interrupts_is_absent_rather_than_wrong_when_the_node_has_none() {
        let fdt = gicv3();
        let root = fdt.root().unwrap();
        assert!(root.interrupts(3).is_none(), "the root declares no interrupts");
    }

    #[test]
    fn reads_psci_the_way_cpu_bringup_will() {
        let fdt = gicv3();
        let psci = fdt.find_compatible("arm,psci-1.0").expect("virt provides PSCI");
        assert_eq!(psci.property("method").unwrap().as_str(), Some("smc"));
        // The function id secondary-CPU bringup will call.
        assert_eq!(psci.property("cpu_on").unwrap().as_u32(), Some(0xc400_0003));
    }

    #[test]
    fn enumerates_cpus_for_smp() {
        let fdt = gicv3();
        let cpus = fdt.find_node("/cpus").expect("virt has /cpus");
        // /cpus overrides the root's cells: addresses are 1 cell, sizes 0.
        assert_eq!(cpus.address_cells(), 1);
        assert_eq!(cpus.size_cells(), 0);

        let ids: Vec<u64> = cpus
            .children()
            .filter(|c| c.base_name() == "cpu")
            .map(|c| c.reg().unwrap().next().unwrap().0)
            .collect();
        // Launched with -smp 4; these are the MPIDR values PSCI CPU_ON takes.
        assert_eq!(ids, vec![0, 1, 2, 3]);
        assert!(fdt.find_node("/cpus/cpu@0").unwrap().is_compatible("arm,cortex-a72"));
        assert_eq!(fdt.boot_cpuid(), 0);
    }

    #[test]
    fn property_accessors_reject_wrong_widths() {
        let fdt = gicv3();
        let root = fdt.root().unwrap();

        let compatible = root.property("compatible").unwrap();
        assert_eq!(compatible.strings().collect::<Vec<_>>(), vec!["linux,dummy-virt"]);
        assert_eq!(compatible.as_u32(), None, "a string is not a cell");

        let cells = root.property("#address-cells").unwrap();
        assert_eq!(cells.as_u32(), Some(2));
        assert_eq!(cells.as_u64(), None, "4 bytes is not a u64");
        assert_eq!(cells.as_uint(), Some(2), "as_uint accepts either width");

        // An empty property is a flag, not a zero.
        let gic = fdt.find_compatible("arm,gic-v3").unwrap();
        let flag = gic.property("interrupt-controller").unwrap();
        assert!(flag.is_empty());
        assert_eq!(flag.as_u32(), None);

        assert!(root.property("does-not-exist").is_none());
    }

    #[test]
    fn multi_string_and_multi_cell_properties() {
        let fdt = gicv3();
        let uart = fdt.find_compatible("arm,pl011").unwrap();

        // `compatible` is a list, most specific first, and matching any entry
        // binds a driver.
        let compatible: Vec<_> = uart.property("compatible").unwrap().strings().collect();
        assert_eq!(compatible, vec!["arm,pl011", "arm,primecell"]);
        assert!(uart.is_compatible("arm,primecell"), "later entries also bind");
        assert!(!uart.is_compatible("arm,pl0"), "prefixes must not match");

        let names: Vec<_> = uart.property("clock-names").unwrap().strings().collect();
        assert_eq!(names, vec!["uartclk", "apb_pclk"]);

        // The PL011's interrupt: SPI 1, level-triggered. `intid 33` (SPI base
        // 32 + 1) is what the driver hard-codes today.
        let interrupts: Vec<_> = uart.property("interrupts").unwrap().cells().collect();
        assert_eq!(interrupts, vec![0, 1, 4]);
    }

    #[test]
    fn walks_children_and_full_tree_consistently() {
        let fdt = gicv3();
        let root = fdt.root().unwrap();

        // Children of the root are a subset of every node, and the two walkers
        // (subtree-skipping vs. linear scan) must agree.
        let children: Vec<_> = root.children().map(|c| c.name()).collect();
        assert!(children.contains(&"memory@40000000"));
        assert!(children.contains(&"chosen"));
        assert!(children.contains(&"cpus"));
        assert!(!children.contains(&"cpu@0"), "grandchildren are not children");

        let all: Vec<_> = fdt.nodes().map(|n| n.name()).collect();
        assert!(all.contains(&"cpu@0"), "the linear walk sees every depth");
        assert!(all.len() > children.len());
        for child in &children {
            assert!(all.contains(child));
        }

        // Several virtio slots exist; find_all must return them all.
        let virtio = fdt.find_all_compatible("virtio,mmio").count();
        assert!(virtio > 1, "virt exposes many virtio-mmio slots, got {virtio}");
    }

    #[test]
    fn chosen_and_reservations() {
        let fdt = gicv3();
        let chosen = fdt.find_node("/chosen").unwrap();
        assert_eq!(chosen.property("stdout-path").unwrap().as_str(), Some("/pl011@9000000"));

        // QEMU passes no command line and no initrd here; the accessors must
        // report absence rather than inventing a value.
        assert!(fdt.bootargs().is_none());
        assert!(fdt.initrd().is_none());

        // virt reserves nothing by either mechanism; a phone will reserve a lot.
        // What matters is that both walks terminate and yield sane pairs.
        for (base, size) in fdt.memory_reservations() {
            assert!(size > 0, "a reservation of nothing is malformed: {base:#x}");
        }
        assert_eq!(fdt.reserved_memory().count(), 0);
    }

    #[test]
    fn from_ptr_reads_the_length_out_of_the_blob() {
        // `include_bytes!` promises no particular alignment, and the spec (and
        // `from_ptr`) require 8 — so stage the blob the way firmware would leave
        // it, aligned.
        let staged = aligned_copy(VIRT_GICV3);
        // SAFETY: `staged` is 8-byte aligned, holds the whole blob, outlives the
        // returned `Fdt`, and is never mutated.
        let fdt = unsafe { Fdt::from_ptr(staged.as_ptr().cast()) }.expect("blob is valid");
        assert_eq!(fdt.total_size(), VIRT_GICV3.len());
        assert_eq!(fdt.find_compatible("arm,pl011").unwrap().reg().unwrap().next().unwrap().0,
                   0x0900_0000);

        let not_a_blob = [0u64; 8];
        // SAFETY: a readable, 8-byte-aligned buffer; `from_ptr` reads only the
        // header before rejecting it on magic.
        let err = unsafe { Fdt::from_ptr(not_a_blob.as_ptr().cast()) }.unwrap_err();
        assert_eq!(err, FdtError::BadMagic);

        // The pointers firmware actually hands over when it has no tree to give,
        // or when we are simply wrong about the boot contract. These must be
        // rejected without ever being dereferenced.
        // SAFETY: nothing is read — both are rejected by the null/alignment
        // check before any access, which is precisely what is under test.
        unsafe {
            assert_eq!(Fdt::from_ptr(core::ptr::null()).unwrap_err(), FdtError::BadMagic);
            let misaligned = VIRT_GICV3.as_ptr().wrapping_add(1);
            assert_eq!(Fdt::from_ptr(misaligned).unwrap_err(), FdtError::BadMagic);
        }
    }
}
