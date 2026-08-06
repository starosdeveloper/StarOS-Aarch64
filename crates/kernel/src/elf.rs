//! A minimal, read-only ELF64 parser — just enough to load a static EL0 program.
//!
//! The kernel's `build.rs` compiles `services/init` into a real ELF executable
//! rather than a flat binary; this module lets the loader honour that format:
//! validate the header, then walk the program headers and hand back each
//! `PT_LOAD` segment (its file image, its virtual address, its memory size, and
//! its `R/W/X` flags) for [`AddressSpace::map_segment`] to place. That replaces
//! the old "copy one page verbatim" loader with something that respects segment
//! boundaries, `.bss`, and per-segment permissions.
//!
//! Deliberately tiny: AArch64, little-endian, `ET_EXEC` only, no relocations, no
//! dynamic linking, no section headers. Every field read is bounds-checked, so a
//! malformed image is rejected rather than trusted.

/// `PT_LOAD` — a segment the loader must place in memory.
const PT_LOAD: u32 = 1;

/// `e_machine` value for AArch64.
const EM_AARCH64: u16 = 183;
/// `e_type` value for a (non-PIE) executable.
const ET_EXEC: u16 = 2;

/// A parsed ELF64 executable borrowing the raw image bytes.
pub struct Elf<'a> {
    image: &'a [u8],
    entry: u64,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

/// One loadable segment (`PT_LOAD`), resolved against the image bytes.
pub struct Segment<'a> {
    /// EL0 virtual address the segment must be mapped at (`p_vaddr`).
    pub vaddr: u64,
    /// The segment's file image: `p_filesz` bytes copied into the mapping. Any
    /// remaining `memsz - file.len()` bytes are the zero-filled `.bss` tail.
    pub file: &'a [u8],
    /// Total in-memory size of the segment (`p_memsz`), ≥ `file.len()`.
    pub memsz: usize,
    /// Segment permission bits (`p_flags`): bit 0 = X, bit 1 = W, bit 2 = R.
    pub flags: u32,
}

/// Read a little-endian `u16`/`u32`/`u64` at `off`, or `None` if out of range.
fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)?.try_into().ok().map(u16::from_le_bytes)
}
fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)?.try_into().ok().map(u32::from_le_bytes)
}
fn read_u64(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)?.try_into().ok().map(u64::from_le_bytes)
}

impl<'a> Elf<'a> {
    /// Validate `image` as an AArch64 ELF64 executable and capture the fields the
    /// loader needs. Returns `None` if the magic, class, endianness, machine or
    /// type is not what we support, or the header is truncated.
    pub fn parse(image: &'a [u8]) -> Option<Self> {
        // e_ident: magic 0x7F 'E' 'L' 'F', then class (2 = ELF64), data (1 = LE).
        if image.get(0..4)? != b"\x7fELF" || image.get(4)? != &2 || image.get(5)? != &1 {
            return None;
        }
        if read_u16(image, 18)? != EM_AARCH64 || read_u16(image, 16)? != ET_EXEC {
            return None;
        }
        Some(Self {
            image,
            entry: read_u64(image, 24)?,
            phoff: read_u64(image, 32)? as usize,
            phentsize: read_u16(image, 54)? as usize,
            phnum: read_u16(image, 56)? as usize,
        })
    }

    /// The program entry point (`e_entry`) — the EL0 VA to start executing at.
    #[must_use]
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// Visit every `PT_LOAD` segment in program-header order, passing each to `f`.
    /// Returns `false` if any header is truncated or a segment's file range lies
    /// outside the image (a malformed ELF); `true` once all are visited.
    pub fn for_each_load(&self, mut f: impl FnMut(Segment<'a>)) -> bool {
        for i in 0..self.phnum {
            let ph = self.phoff + i * self.phentsize;
            // Program header (Elf64_Phdr): type@0, flags@4, offset@8, vaddr@16,
            // filesz@32, memsz@40.
            let (Some(p_type), Some(p_flags)) = (read_u32(self.image, ph), read_u32(self.image, ph + 4))
            else {
                return false;
            };
            if p_type != PT_LOAD {
                continue;
            }
            let (Some(offset), Some(vaddr), Some(filesz), Some(memsz)) = (
                read_u64(self.image, ph + 8).map(|v| v as usize),
                read_u64(self.image, ph + 16),
                read_u64(self.image, ph + 32).map(|v| v as usize),
                read_u64(self.image, ph + 40).map(|v| v as usize),
            ) else {
                return false;
            };
            if filesz > memsz {
                return false;
            }
            let Some(file) = self.image.get(offset..offset + filesz) else {
                return false;
            };
            f(Segment { vaddr, file, memsz, flags: p_flags });
        }
        true
    }
}
