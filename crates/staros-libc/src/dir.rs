//! Directories, over a flat archive.
//!
//! The initramfs is a CPIO archive and CPIO stores *paths*: `fonts/DejaVu.ttf` is a
//! member, `fonts` is not. A directory here is therefore a prefix that some member
//! uses, and `opendir("fonts")` succeeds exactly when something lives under it.
//!
//! That makes `readdir` a filter rather than a lookup: the client walks the
//! archive's members by index (`TAG_LIST`), keeps the ones under the prefix, and
//! reports the first path component after it. Two members `fonts/a.ttf` and
//! `fonts/b/c.ttf` produce three entries between them — `a.ttf`, and `b` once,
//! reported as a directory. Suppressing the repeat is the only state a `DIR` keeps
//! beyond its index, and it is why a subdirectory does not appear once per file
//! inside it.
//!
//! Everything is read-only, so there is no `mkdir` here; the refusals live with the
//! other filesystem writes in [`crate::file`].

use core::ffi::{c_char, c_int, c_uchar};

use staros_abi::fsproto::MAX_PATH;

/// The longest name `readdir` reports. POSIX says a `dirent` is a fixed structure
/// with a fixed name array, and this is that array.
const NAME_MAX: usize = 255;

/// `struct dirent`, in glibc's AArch64 layout — the same reasoning as `struct
/// stat`: the programs that read it were compiled against glibc's headers.
#[repr(C)]
pub struct Dirent {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: c_uchar,
    pub d_name: [c_char; NAME_MAX + 1],
}

const _: () = {
    assert!(core::mem::offset_of!(Dirent, d_reclen) == 16);
    assert!(core::mem::offset_of!(Dirent, d_type) == 18);
    assert!(core::mem::offset_of!(Dirent, d_name) == 19);
};

/// `d_type` values. Only these two occur here.
const DT_DIR: c_uchar = 4;
const DT_REG: c_uchar = 8;

/// An open directory: the prefix it names, how far through the archive it has read,
/// and the entry `readdir` last returned.
///
/// The `dirent` is *inside* the `DIR`, because C says the pointer `readdir` returns
/// stays valid until the next call on the same directory and no longer. A shared
/// static would make two directories overwrite each other's results, which is the
/// kind of bug that only appears once a program lists two things at once.
#[repr(C)]
pub struct Dir {
    /// The prefix, without leading or trailing slashes.
    prefix: [u8; MAX_PATH],
    prefix_len: usize,
    /// The next archive index to examine.
    index: u64,
    /// Names already reported, so a subdirectory appears once rather than once per
    /// file beneath it. Bounded: past this many, repeats are possible again, and
    /// that is better than an allocation that can fail in the middle of a listing.
    seen: [[u8; 32]; 32],
    seen_len: [u8; 32],
    seen_count: usize,
    entry: Dirent,
}

/// The entry an archive member contributes to a listing of `prefix`, if any.
///
/// This is the whole of `readdir`'s decision, kept out of the C layer so it can be
/// checked without a file server: is this member under the prefix, and is what it
/// contributes a file or the name of a subdirectory? `Some((name, true))` means a
/// directory, which the caller reports once however many members are beneath it.
pub(crate) fn component_under<'a>(prefix: &[u8], entry: &'a [u8]) -> Option<(&'a [u8], bool)> {
    let entry = crate::file::trim_slashes(entry);
    let rest = if prefix.is_empty() {
        entry
    } else if entry.len() > prefix.len()
        && entry.starts_with(prefix)
        && entry[prefix.len()] == b'/'
    {
        &entry[prefix.len() + 1..]
    } else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    match rest.iter().position(|&c| c == b'/') {
        Some(cut) => Some((&rest[..cut], true)),
        None => Some((rest, false)),
    }
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_char, c_int, Dir, Dirent, DT_DIR, DT_REG, MAX_PATH, NAME_MAX};
    use crate::fail;

    const ENOENT: c_int = 2;
    const ENOMEM: c_int = 12;
    const EBADF: c_int = 9;

    /// # Safety
    /// C ABI: `path` is NUL-terminated.
    #[no_mangle]
    pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut Dir {
        // SAFETY: forwarded from the caller.
        let raw = unsafe { crate::string::as_bytes(path) };
        let prefix = crate::file::trim_slashes(raw);
        if prefix.len() > MAX_PATH {
            return fail(ENOENT, core::ptr::null_mut());
        }
        if !crate::file::directory_exists(raw) {
            return fail(ENOENT, core::ptr::null_mut());
        }
        let dir = crate::heap_alloc(core::mem::size_of::<Dir>(), 8).cast::<Dir>();
        if dir.is_null() {
            return fail(ENOMEM, core::ptr::null_mut());
        }
        // SAFETY: `dir` is a fresh allocation of exactly one `Dir`. Writing the
        // whole structure rather than assigning fields is what initialises the
        // `dirent` and the seen-list; a partially written `Dir` would have
        // `readdir` reading uninitialised names.
        unsafe {
            dir.write(Dir {
                prefix: [0; MAX_PATH],
                prefix_len: prefix.len(),
                index: 0,
                seen: [[0; 32]; 32],
                seen_len: [0; 32],
                seen_count: 0,
                entry: Dirent {
                    d_ino: 0,
                    d_off: 0,
                    d_reclen: core::mem::size_of::<Dirent>() as u16,
                    d_type: DT_REG,
                    d_name: [0; NAME_MAX + 1],
                },
            });
            (&mut (*dir).prefix)[..prefix.len()].copy_from_slice(prefix);
        }
        dir
    }

    /// # Safety
    /// C ABI: `dir` came from [`opendir`].
    #[no_mangle]
    pub unsafe extern "C" fn readdir(dir: *mut Dir) -> *mut Dirent {
        if dir.is_null() {
            return fail(EBADF, core::ptr::null_mut());
        }
        // SAFETY: the caller's contract.
        let d = unsafe { &mut *dir };
        let mut name = [0u8; MAX_PATH];
        loop {
            // The size and mode the server reports belong to `stat`; a `dirent`
            // carries neither, only the kind.
            let Some((len, _, _)) = crate::file::list(d.index, &mut name) else {
                return core::ptr::null_mut(); // end of the archive
            };
            d.index += 1;
            // Everything under the prefix, and nothing else; the first path
            // component of what remains. An empty prefix is the root, which every
            // member is under.
            let Some((component, is_dir)) =
                super::component_under(&d.prefix[..d.prefix_len], &name[..len])
            else {
                continue;
            };
            if is_dir && already_seen(d, component) {
                continue;
            }
            if component.len() > NAME_MAX {
                continue;
            }
            d.entry.d_type = if is_dir { DT_DIR } else { DT_REG };
            d.entry.d_ino = d.index; // an index is not an inode, but it is unique
            d.entry.d_off = d.index as i64;
            for (i, &b) in component.iter().enumerate() {
                d.entry.d_name[i] = b as c_char;
            }
            d.entry.d_name[component.len()] = 0;
            return core::ptr::addr_of_mut!(d.entry);
        }
    }

    /// Remember a directory name, and report whether it was already there.
    fn already_seen(d: &mut Dir, name: &[u8]) -> bool {
        let len = name.len().min(32);
        for i in 0..d.seen_count {
            if usize::from(d.seen_len[i]) == len && d.seen[i][..len] == name[..len] {
                return true;
            }
        }
        if d.seen_count < d.seen.len() {
            d.seen[d.seen_count][..len].copy_from_slice(&name[..len]);
            d.seen_len[d.seen_count] = len as u8;
            d.seen_count += 1;
        }
        false
    }

    /// # Safety
    /// As [`readdir`].
    #[no_mangle]
    pub unsafe extern "C" fn readdir64(dir: *mut Dir) -> *mut Dirent {
        // SAFETY: forwarded from the caller.
        unsafe { readdir(dir) }
    }

    /// # Safety
    /// As [`readdir`].
    #[no_mangle]
    pub unsafe extern "C" fn rewinddir(dir: *mut Dir) {
        if dir.is_null() {
            return;
        }
        // SAFETY: the caller's contract.
        unsafe {
            (*dir).index = 0;
            (*dir).seen_count = 0;
        }
    }

    /// # Safety
    /// As [`readdir`].
    #[no_mangle]
    pub unsafe extern "C" fn closedir(dir: *mut Dir) -> c_int {
        if dir.is_null() {
            return fail(EBADF, -1);
        }
        // SAFETY: the allocation came from `opendir`.
        unsafe { crate::exports::free(dir.cast::<core::ffi::c_void>()) };
        0
    }

    /// `dirfd`: a directory here is not a descriptor — there is no handle in the
    /// file server behind it, only an index this process keeps. Refusing is the
    /// honest answer; returning some other descriptor would let a caller `fstat`
    /// the wrong thing.
    ///
    /// # Safety
    /// C ABI.
    #[no_mangle]
    pub unsafe extern "C" fn dirfd(_dir: *mut Dir) -> c_int {
        fail(EBADF, -1)
    }
}

#[cfg(test)]
mod tests {
    use super::component_under;

    /// The archive that is actually in the initramfs is flat like this one: paths,
    /// with no entry for the directories they imply.
    const ARCHIVE: [&[u8]; 6] = [
        b"hello.txt",
        b"fonts/DejaVuSans.ttf",
        b"fonts/DejaVuSerif.ttf",
        b"fonts/extra/Noto.ttf",
        b"fonts.txt",
        b"bin/qml",
    ];

    /// What a `readdir` loop over the archive would report, suppressing the repeats
    /// the same way [`super::exports::readdir`] does.
    fn listing(prefix: &[u8]) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = Vec::new();
        for entry in ARCHIVE {
            let Some((name, is_dir)) = component_under(prefix, entry) else {
                continue;
            };
            let name = String::from_utf8(name.to_vec()).unwrap();
            if is_dir && out.iter().any(|(seen, _)| *seen == name) {
                continue;
            }
            out.push((name, is_dir));
        }
        out
    }

    #[test]
    fn the_root_lists_top_level_names_once() {
        assert_eq!(
            listing(b""),
            vec![
                ("hello.txt".into(), false),
                ("fonts".into(), true),
                ("fonts.txt".into(), false),
                ("bin".into(), true),
            ]
        );
    }

    #[test]
    fn a_subdirectory_is_reported_once_not_once_per_file() {
        // `fonts/extra/Noto.ttf` is the only member under `fonts/extra`, but were
        // there ten of them, `extra` would still appear a single time.
        assert_eq!(
            listing(b"fonts"),
            vec![
                ("DejaVuSans.ttf".into(), false),
                ("DejaVuSerif.ttf".into(), false),
                ("extra".into(), true),
            ]
        );
    }

    #[test]
    fn a_prefix_stops_at_the_separator() {
        // The bug this guards: `fonts.txt` starts with `fonts`, and a listing that
        // compares bytes without demanding the `/` would put it inside the
        // directory, under the name `.txt`.
        assert_eq!(component_under(b"fonts", b"fonts.txt"), None);
        assert_eq!(component_under(b"font", b"fonts/DejaVuSans.ttf"), None);
    }

    #[test]
    fn a_member_is_not_inside_itself() {
        assert_eq!(component_under(b"hello.txt", b"hello.txt"), None);
    }

    #[test]
    fn the_leading_slash_and_dot_forms_name_the_same_thing() {
        assert_eq!(
            component_under(b"fonts", b"/fonts/DejaVuSans.ttf"),
            Some((&b"DejaVuSans.ttf"[..], false))
        );
        assert_eq!(
            component_under(b"fonts", b"./fonts/extra/Noto.ttf"),
            Some((&b"extra"[..], true))
        );
    }

    #[test]
    fn trailing_slashes_do_not_produce_an_empty_name() {
        assert_eq!(component_under(b"fonts", b"fonts/"), None);
        assert_eq!(component_under(b"", b"/"), None);
    }
}
