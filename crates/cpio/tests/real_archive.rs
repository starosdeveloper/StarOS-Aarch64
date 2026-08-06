//! Parse a *real* `cpio -o -H newc` archive, the same way the fdt crate tests
//! against real DTBs rather than hand-built ones. `tests/data/sample.cpio` was
//! produced by GNU cpio 2.15 (`--reproducible`) from three files.

use staros_cpio::Archive;

const SAMPLE: &[u8] = include_bytes!("data/sample.cpio");

#[test]
fn reads_every_member_of_a_real_archive() {
    let archive = Archive::new(SAMPLE);
    let names: Vec<&str> = archive.entries().map(|e| e.name).collect();
    assert_eq!(names, vec!["greeting.txt", "etc_version", "bin/hello"]);
}

#[test]
fn borrows_file_contents_verbatim() {
    let archive = Archive::new(SAMPLE);
    let greeting = archive.find("greeting.txt").expect("greeting present");
    assert_eq!(greeting.data, b"hello from the initramfs\n");
    assert!(greeting.is_file());

    let version = archive.find("etc_version").expect("version present");
    assert_eq!(version.data, b"STAR OS 0.2.0\n");
}

#[test]
fn a_missing_name_is_none_not_a_panic() {
    let archive = Archive::new(SAMPLE);
    assert!(archive.find("/no/such/file").is_none());
}

#[test]
fn a_nested_path_keeps_its_directory_prefix() {
    // newc stores full relative paths; the reader does not flatten them.
    let archive = Archive::new(SAMPLE);
    assert!(archive.find("bin/hello").is_some());
    assert!(archive.find("hello").is_none());
}

#[test]
fn corrupting_any_single_byte_never_panics() {
    // The archive is untrusted input; a one-byte flip must at worst end iteration,
    // never panic or read out of bounds.
    for i in 0..SAMPLE.len() {
        let mut blob = SAMPLE.to_vec();
        blob[i] ^= 0xff;
        let archive = Archive::new(&blob);
        // Draining the iterator must not panic; we don't care what it yields.
        let _ = archive.entries().count();
        let _ = archive.find("greeting.txt");
    }
}
