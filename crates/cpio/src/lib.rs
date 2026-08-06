//! CPIO "newc" archive reader — the initramfs format, parsed in user space.
//!
//! An initramfs is how the first real filesystem arrives: the bootloader loads a
//! CPIO archive into RAM and points the kernel at it (`/chosen/linux,initrd-*`),
//! the kernel maps it read-only into the first user process, and that process
//! unpacks it — a filesystem with **zero storage-driver code** (roadmap 2.4). The
//! "newc" format (what `cpio -o -H newc` and Linux's `gen_init_cpio` produce) is a
//! sequence of entries, each an ASCII-hex header, a NUL-terminated name, and file
//! data, both padded to a 4-byte boundary, ending with an entry named
//! `TRAILER!!!`.
//!
//! This reader borrows every name and data slice straight out of the archive —
//! no allocation, no copying — and treats any malformed field as end-of-archive
//! rather than panicking: the archive is untrusted input a bootloader handed us.

#![cfg_attr(not(test), no_std)]

/// The "newc" magic every header starts with.
const MAGIC: &[u8] = b"070701";
/// A newc header is 13 ASCII-hex fields of 8 chars after the 6-char magic = 110.
const HEADER_LEN: usize = 110;
/// Fields (name, data) are padded so their end aligns to this boundary.
const ALIGN: usize = 4;
/// The sentinel name that ends the archive.
const TRAILER: &str = "TRAILER!!!";

/// Offsets of the two header fields we use, in bytes from the header start. Every
/// field is 8 ASCII-hex chars; we only need the name and file sizes and the mode.
const OFF_MODE: usize = 14; // c_mode
const OFF_FILESIZE: usize = 54; // c_filesize
const OFF_NAMESIZE: usize = 94; // c_namesize

/// One archive member: its path, mode bits, and file contents (borrowed from the
/// archive, never copied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// The member's name (path), without the trailing NUL.
    pub name: &'a str,
    /// The `c_mode` field — file type + permission bits, as in `stat`.
    pub mode: u32,
    /// The file's contents.
    pub data: &'a [u8],
}

impl Entry<'_> {
    /// Is this a regular file? (`S_IFMT` bits == `S_IFREG`.) Directories and
    /// symlinks in an archive are not files to read.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
}

/// A borrowed CPIO archive, iterable into [`Entry`]s.
#[derive(Clone, Copy)]
pub struct Archive<'a> {
    blob: &'a [u8],
}

impl<'a> Archive<'a> {
    /// Wrap a byte slice as an archive. Cheap — validation happens per entry as
    /// you iterate, so a bad archive simply yields nothing.
    #[must_use]
    pub const fn new(blob: &'a [u8]) -> Self {
        Self { blob }
    }

    /// Iterate the archive's entries, stopping at the `TRAILER!!!` sentinel or at
    /// the first malformed header.
    #[must_use]
    pub const fn entries(&self) -> Entries<'a> {
        Entries { blob: self.blob, pos: 0 }
    }

    /// Find a member by exact name, or `None`. This is the whole "open a file"
    /// primitive an initramfs needs.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<Entry<'a>> {
        self.entries().find(|e| e.name == name)
    }
}

/// Iterator over an [`Archive`]'s entries.
pub struct Entries<'a> {
    blob: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    fn next(&mut self) -> Option<Entry<'a>> {
        let entry = parse_at(self.blob, self.pos)?;
        // Advance past this entry to the next 4-aligned header.
        let name_end = self.pos + HEADER_LEN + entry.namesize;
        let data_start = align_up(name_end);
        let data_end = data_start.checked_add(entry.filesize)?;
        self.pos = align_up(data_end);

        if entry.name == TRAILER {
            return None; // end of archive
        }
        Some(Entry { name: entry.name, mode: entry.mode, data: entry.data })
    }
}

/// A parsed header plus the borrowed name/data and the raw sizes we need to
/// advance. Split out so the iterator can both yield the entry and compute the
/// next position.
struct Parsed<'a> {
    name: &'a str,
    mode: u32,
    data: &'a [u8],
    namesize: usize,
    filesize: usize,
}

/// Parse the entry whose header begins at `pos`, or `None` if the bytes there are
/// not a well-formed newc header/name/data within the blob's bounds.
fn parse_at(blob: &[u8], pos: usize) -> Option<Parsed<'_>> {
    let header = blob.get(pos..pos + HEADER_LEN)?;
    if &header[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mode = hex_field(header, OFF_MODE)?;
    let filesize = hex_field(header, OFF_FILESIZE)? as usize;
    let namesize = hex_field(header, OFF_NAMESIZE)? as usize;

    // The name includes a trailing NUL counted in namesize; borrow it minus the
    // NUL and reject a zero-length or non-UTF-8 name.
    let name_start = pos + HEADER_LEN;
    let name_bytes = blob.get(name_start..name_start + namesize)?;
    let name = core::str::from_utf8(name_bytes.split_last()?.1).ok()?;

    let data_start = align_up(name_start + namesize);
    let data = blob.get(data_start..data_start + filesize)?;

    Some(Parsed { name, mode, data, namesize, filesize })
}

/// Read an 8-character ASCII-hex field at byte offset `off` of a header.
fn hex_field(header: &[u8], off: usize) -> Option<u32> {
    let bytes = header.get(off..off + 8)?;
    let mut v: u32 = 0;
    for &b in bytes {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | u32::from(d);
    }
    Some(v)
}

/// Round `n` up to the next multiple of [`ALIGN`].
const fn align_up(n: usize) -> usize {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real archive built at test time by `build.rs`? No — keep the crate
    // dependency-free and commit a real `cpio -o -H newc` blob as a fixture, the
    // same way the fdt crate commits real DTBs. See tests/data/sample.cpio and the
    // integration test in tests/.
    #[test]
    fn align_rounds_up_to_four() {
        assert_eq!(align_up(0), 0);
        assert_eq!(align_up(1), 4);
        assert_eq!(align_up(4), 4);
        assert_eq!(align_up(5), 8);
    }

    #[test]
    fn a_hand_built_single_file_archive_round_trips() {
        // Construct a minimal newc archive by hand so the parser is exercised with
        // no external fixture: one file "hi" containing "yo", then the trailer.
        let mut a = alloc_archive();
        push_entry(&mut a, "hi", 0o100644, b"yo");
        push_entry(&mut a, TRAILER, 0, b"");

        let archive = Archive::new(&a);
        let hi = archive.find("hi").expect("file present");
        assert_eq!(hi.data, b"yo");
        assert!(hi.is_file());
        assert_eq!(hi.name, "hi");
        // The trailer is not yielded.
        assert_eq!(archive.entries().count(), 1);
        assert!(archive.find("nope").is_none());
    }

    #[test]
    fn a_truncated_entry_ends_iteration_without_panicking() {
        let mut a = alloc_archive();
        push_entry(&mut a, "first", 0o100644, b"aaaa");
        let after_first = a.len();
        push_entry(&mut a, "second", 0o100644, b"bbbbbbbb");
        // Chop into the second entry's data — the first stays whole, the second
        // no longer fits its declared filesize, so iteration must stop cleanly.
        a.truncate(after_first + HEADER_LEN + 4);
        let archive = Archive::new(&a);
        assert_eq!(archive.entries().count(), 1);
        assert_eq!(archive.find("first").unwrap().data, b"aaaa");
    }

    #[test]
    fn garbage_yields_nothing() {
        assert_eq!(Archive::new(b"not a cpio archive at all").entries().count(), 0);
        assert_eq!(Archive::new(&[]).entries().count(), 0);
    }

    // --- test helpers: build a newc archive in a Vec (test-only, so `std`) ------
    fn alloc_archive() -> Vec<u8> {
        Vec::new()
    }

    fn push_entry(buf: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
        let namesize = name.len() + 1; // includes NUL
        let write_hex = |buf: &mut Vec<u8>, v: u32| {
            let s = format!("{v:08X}");
            buf.extend_from_slice(s.as_bytes());
        };
        buf.extend_from_slice(MAGIC);
        for field in [
            0,               // ino
            mode,            // mode
            0,               // uid
            0,               // gid
            1,               // nlink
            0,               // mtime
            data.len() as u32, // filesize
            0,               // devmajor
            0,               // devminor
            0,               // rdevmajor
            0,               // rdevminor
            namesize as u32, // namesize
            0,               // check
        ] {
            write_hex(buf, field);
        }
        buf.extend_from_slice(name.as_bytes());
        buf.push(0);
        while buf.len() % ALIGN != 0 {
            buf.push(0);
        }
        buf.extend_from_slice(data);
        while buf.len() % ALIGN != 0 {
            buf.push(0);
        }
    }
}
