//! Build the `init` boot image and hand its ELF executable to the kernel.
//!
//! `init` is a *separately compiled* EL0 program, not code baked into the
//! kernel's own `.text`. This script compiles `services/init/boot/image.rs` with
//! a plain `rustc` invocation for `aarch64-unknown-none` — using that program's
//! own linker script (which places it at `USER_BASE` with separate R-X and R-W
//! `PT_LOAD` segments) — and exports the path to the resulting **ELF** as the
//! `STAROS_INIT_IMAGE` env var. `main.rs` `include_bytes!`s it and the kernel's
//! ELF loader parses the program headers, mapping each segment with its own
//! rights at runtime. (Earlier revisions flattened the ELF to a raw blob with
//! `llvm-objcopy`; the real loader makes that step unnecessary.)
//!
//! We call `rustc` directly (rather than a nested `cargo`) deliberately: the
//! program is a single `no_std`, naked-`_start` file with no dependencies and no
//! need for `core`'s memory intrinsics, so the target's *pre-compiled* `core` is
//! enough — no `build-std`, no nested workspace/lockfile, no target-dir lock.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());

    // Sources for the init boot image, relative to this crate (crates/kernel).
    // Canonicalize so the `rerun-if-changed` paths cargo tracks are exactly the
    // ones it stats — no `..` components that could ever fail to match.
    let boot_dir = Path::new(&manifest_dir).join("../../services/init/boot");
    let image_rs = canonical(&boot_dir.join("image.rs"));
    let image_ld = canonical(&boot_dir.join("image.ld"));

    println!("cargo:rerun-if-changed={}", image_rs.display());
    println!("cargo:rerun-if-changed={}", image_ld.display());

    let elf = Path::new(&out_dir).join("init.elf");

    // Compile the init program to an ELF linked at USER_BASE. The target's
    // pre-compiled `core` satisfies a dependency-free naked-`_start` program, so
    // no `-Zbuild-std` is required.
    //
    // `-z max-page-size=4096` (as two linker tokens — lld does not parse the
    // glued `-zmax-page-size=…` form) is a *size* optimization, not a correctness
    // requirement: the kernel's ELF loader reads `p_vaddr`/`p_offset` directly and
    // the linker script pins each segment's VA with `ALIGN(0x1000)`, so the image
    // loads correctly regardless. Without the flag lld defaults to a 64 KiB max
    // page size and pads the file with ~64 KiB of zeroes between the R-X and R-W
    // segments — bytes that would otherwise be baked into the kernel by
    // `include_bytes!`. The `MAX_IMAGE_BYTES` guard below fails the build loudly
    // if that padding (or any other regression) ever creeps back in.
    let status = Command::new(&rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_init_image"])
        .args(["--crate-type", "bin"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&elf)
        .arg(&image_rs)
        .status()
        .expect("failed to spawn rustc for the init image");
    assert!(status.success(), "rustc failed to build the init image");

    // Fail fast if the linked image is unexpectedly large. The init program is a
    // few hundred bytes of code plus a page-aligned data segment; anything past
    // this bound means the segment layout regressed (e.g. the max-page-size flag
    // stopped taking effect and the file filled with inter-segment padding).
    const MAX_IMAGE_BYTES: u64 = 32 * 1024;
    let size = std::fs::metadata(&elf)
        .expect("init image was not produced")
        .len();
    assert!(
        size <= MAX_IMAGE_BYTES,
        "init image is {size} bytes (> {MAX_IMAGE_BYTES}); the segment layout \
         regressed — check the `-z max-page-size=4096` linker flag",
    );

    // Publish the ELF path for `include_bytes!` in the kernel; the ELF loader
    // parses it directly.
    println!("cargo:rustc-env=STAROS_INIT_IMAGE={}", elf.display());

    build_devicemgr(&manifest_dir, &out_dir, &rustc, &image_ld);
}

/// Build the `devicemgr` EL0 program and publish its ELF path.
///
/// Unlike `init`, this program links against the `fdt` crate, so the build is two
/// `rustc` steps: compile `fdt` to an rlib for the bare-metal target, then compile
/// `devicemgr` against it with `--extern`. Still no `-Zbuild-std`: at
/// `-Copt-level=2` the compiler inlines `fdt`'s slice work, so `core`'s memory
/// intrinsics are never referenced and the pre-compiled `core` is enough.
fn build_devicemgr(manifest_dir: &str, out_dir: &str, rustc: &str, image_ld: &Path) {
    let fdt_src = canonical(&Path::new(manifest_dir).join("../fdt/src/lib.rs"));
    let cpio_src = canonical(&Path::new(manifest_dir).join("../cpio/src/lib.rs"));
    let dm_src = canonical(&Path::new(manifest_dir).join("../../services/devicemgr/main.rs"));
    println!("cargo:rerun-if-changed={}", fdt_src.display());
    println!("cargo:rerun-if-changed={}", cpio_src.display());
    println!("cargo:rerun-if-changed={}", dm_src.display());

    // Step 1: fdt and cpio → rlibs. Both crates are dependency-free and `no_std`.
    let fdt_rlib = Path::new(out_dir).join("libstaros_fdt.rlib");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_fdt"])
        .args(["--crate-type", "lib"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        .arg("-o")
        .arg(&fdt_rlib)
        .arg(&fdt_src)
        .status()
        .expect("failed to spawn rustc for the fdt rlib");
    assert!(status.success(), "rustc failed to build the fdt rlib");

    let cpio_rlib = Path::new(out_dir).join("libstaros_cpio.rlib");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_cpio"])
        .args(["--crate-type", "lib"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        .arg("-o")
        .arg(&cpio_rlib)
        .arg(&cpio_src)
        .status()
        .expect("failed to spawn rustc for the cpio rlib");
    assert!(status.success(), "rustc failed to build the cpio rlib");

    // Step 2: devicemgr → ELF, linked at USER_BASE with the same script as init
    // (its own address space, so the shared link address is fine).
    let dm_elf = Path::new(out_dir).join("devicemgr.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_devicemgr"])
        .args(["--crate-type", "bin"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        // Strip symbols and debug info: unstripped, the `fdt`/`cpio` debug data
        // balloons the ELF to hundreds of KiB the kernel would `include_bytes!`
        // verbatim. The loadable segments are unaffected — the kernel maps those,
        // not the symbol tables.
        .arg("-Cstrip=symbols")
        .arg("--extern")
        .arg(format!("staros_fdt={}", fdt_rlib.display()))
        .arg("--extern")
        .arg(format!("staros_cpio={}", cpio_rlib.display()))
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&dm_elf)
        .arg(&dm_src)
        .status()
        .expect("failed to spawn rustc for devicemgr");
    assert!(status.success(), "rustc failed to build devicemgr");

    // A parsing program is larger than a naked `_start`, but must still be a
    // handful of pages, not the debug-padded ~700 KiB an unstripped link produces
    // — the loadable segments are what the kernel embeds and maps.
    const MAX_DEVICEMGR_BYTES: u64 = 128 * 1024;
    let size = std::fs::metadata(&dm_elf)
        .expect("devicemgr image was not produced")
        .len();
    assert!(
        size <= MAX_DEVICEMGR_BYTES,
        "devicemgr image is {size} bytes (> {MAX_DEVICEMGR_BYTES}); the layout regressed",
    );

    println!("cargo:rustc-env=STAROS_DEVICEMGR_IMAGE={}", dm_elf.display());
}

/// Canonicalize a path, panicking with a clear message if it does not exist —
/// used for the init sources so a mislaid file fails the build immediately rather
/// than silently dropping a `rerun-if-changed` dependency.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("init image source {} not found: {e}", path.display()))
}
