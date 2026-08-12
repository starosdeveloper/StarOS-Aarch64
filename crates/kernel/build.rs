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

    // The device manager builds the `cpio` rlib on its way; the file server links
    // against that same artefact rather than compiling a second copy — and takes
    // its path as an argument, so the ordering these two lines encode is visible
    // instead of being an implicit "must run after".
    // Every service below is compiled *unstripped* to `<name>.debug.elf` and then
    // stripped into the `<name>.elf` the kernel embeds. Two files rather than one
    // because the two consumers want opposite things: the image wants no symbol
    // table, and `scripts/symbolize.sh` wants nothing else — an address in a fault
    // backtrace is only a name if something on the host still knows the names.
    let objcopy = llvm_objcopy();
    // The ABI crate as an rlib for the bare-metal target: the file server and the C
    // library both speak the protocol in `staros_abi::fsproto`, and a protocol
    // written down twice is a protocol that will disagree with itself.
    let abi_rlib = build_rlib(
        &out_dir,
        &rustc,
        "staros_abi",
        &canonical(&Path::new(&manifest_dir).join("../abi/src/lib.rs")),
    );
    let cpio_rlib = build_devicemgr(&manifest_dir, &out_dir, &rustc, &image_ld, &objcopy);
    build_displaysrv(&manifest_dir, &out_dir, &rustc, &image_ld, &objcopy);
    build_inputsrv(&manifest_dir, &out_dir, &rustc, &image_ld, &objcopy);
    build_fssrv(&manifest_dir, &out_dir, &rustc, &image_ld, &objcopy, &cpio_rlib, &abi_rlib);
    build_fsclient(&manifest_dir, &out_dir, &rustc, &image_ld, &objcopy);
    // The C library, and a C program linked against it. This is the toolchain Qt
    // will arrive through, exercised by something small enough to debug.
    let libc = build_libc(&manifest_dir, &out_dir, &rustc, &abi_rlib);
    build_hello_c(&manifest_dir, &out_dir, &image_ld, &objcopy, &libc);
}

/// Compile one dependency-free crate of ours to an rlib for the bare-metal target.
///
/// Plain `rustc`, no `-Zbuild-std`: at `-Copt-level=2` these crates reference none
/// of `core`'s memory intrinsics, so the target's precompiled `core` is enough.
fn build_rlib(out_dir: &str, rustc: &str, crate_name: &str, src: &Path) -> PathBuf {
    println!("cargo:rerun-if-changed={}", src.display());
    let rlib = Path::new(out_dir).join(format!("lib{crate_name}.rlib"));
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", crate_name])
        .args(["--crate-type", "lib"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        .arg("-o")
        .arg(&rlib)
        .arg(src)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn rustc for {crate_name}: {e}"));
    assert!(status.success(), "rustc failed to build the {crate_name} rlib");
    rlib
}

/// Build `crates/staros-libc` as a static library C can link against.
///
/// `staticlib` rather than `rlib` because the consumer is a C linker: it wants an
/// archive of objects with C symbol names, and it must find `memcpy` and friends in
/// it. Returns the archive's path.
fn build_libc(manifest_dir: &str, out_dir: &str, rustc: &str, abi_rlib: &Path) -> PathBuf {
    let src_dir = Path::new(manifest_dir).join("../staros-libc/src");
    let src = canonical(&src_dir.join("lib.rs"));
    // Every module, so editing one of them rebuilds the archive. `rustc` is invoked
    // on the crate root, and cargo only re-runs this script for files it is told
    // about.
    for module in ["lib.rs", "fmt.rs", "file.rs", "heap.rs", "stdio.rs", "string.rs", "sys.rs", "time.rs"] {
        println!("cargo:rerun-if-changed={}", canonical(&src_dir.join(module)).display());
    }

    let lib = Path::new(out_dir).join("libstaros_libc.a");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_libc"])
        .args(["--crate-type", "staticlib"])
        .args(SERVICE_FLAGS)
        .arg("--extern")
        .arg(format!("staros_abi={}", abi_rlib.display()))
        .arg("-o")
        .arg(&lib)
        .arg(&src)
        .status()
        .expect("failed to spawn rustc for staros-libc");
    assert!(status.success(), "rustc failed to build staros-libc");
    lib
}

/// Compile `services/hello-c/main.c` with clang and link it against the C library.
///
/// This is the path Qt will arrive through, which is why it uses the real tools
/// rather than a Rust stand-in: clang for the C, `rust-lld` for the link, the same
/// `image.ld` every EL0 program uses. `-ffreestanding` says there is no hosted
/// environment; `-fno-builtin` keeps clang from turning a loop into a call to a
/// `memcpy` it then assumes exists in a libc it knows — ours is the only one here,
/// and it is linked in explicitly.
///
/// If clang is not installed the build says so and continues without the program;
/// the kernel then reports that there is no C demo, rather than failing to build
/// for everyone who has no C compiler.
fn build_hello_c(
    manifest_dir: &str,
    out_dir: &str,
    image_ld: &Path,
    objcopy: &Path,
    libc: &Path,
) {
    let src = canonical(&Path::new(manifest_dir).join("../../services/hello-c/main.c"));
    println!("cargo:rerun-if-changed={}", src.display());

    let Ok(clang) = which("clang") else {
        // Point the kernel at an empty file rather than at nothing: `include_bytes!`
        // needs a path that exists, and an empty image is a thing the kernel can
        // check for and report. A `cfg` flag would work too and would mean the
        // absence of a C compiler changes which code the kernel contains.
        println!("cargo:warning=clang not found; the C demo program will be absent");
        let empty = Path::new(out_dir).join("hello-c.absent");
        std::fs::write(&empty, []).expect("failed to write the placeholder image");
        println!("cargo:rustc-env=STAROS_HELLO_C_IMAGE={}", empty.display());
        return;
    };
    let object = Path::new(out_dir).join("hello-c.o");
    let status = Command::new(clang)
        .args(["--target=aarch64-unknown-none", "-ffreestanding", "-fno-builtin"])
        .args(["-fno-omit-frame-pointer", "-fno-stack-protector", "-fno-pie"])
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-std=c11", "-c"])
        .arg(&src)
        .arg("-o")
        .arg(&object)
        .status()
        .expect("failed to spawn clang");
    assert!(status.success(), "clang failed to compile hello-c");

    // `rust-lld` ships with the toolchain, so the C program needs no cross binutils
    // either. `--gc-sections` matters more here than elsewhere: the C library
    // archive carries `core`'s formatting machinery, and without it a 200-line
    // program links half a megabyte.
    let lld = llvm_tool("rust-lld").expect("rust-lld is part of the Rust toolchain");
    let debug_elf = Path::new(out_dir).join("hello-c.debug.elf");
    let elf = Path::new(out_dir).join("hello-c.elf");
    let status = Command::new(lld)
        .args(["-flavor", "gnu"])
        .arg(format!("-T{}", image_ld.display()))
        .arg("-z")
        .arg("max-page-size=4096")
        .arg("--gc-sections")
        .arg("-o")
        .arg(&debug_elf)
        .arg(&object)
        .arg(libc)
        .status()
        .expect("failed to spawn rust-lld");
    assert!(status.success(), "rust-lld failed to link hello-c");
    strip_to(objcopy, &debug_elf, &elf);

    const MAX_HELLO_C_BYTES: u64 = 256 * 1024;
    let size = std::fs::metadata(&elf).expect("hello-c image was not produced").len();
    assert!(
        size <= MAX_HELLO_C_BYTES,
        "hello-c image is {size} bytes (> {MAX_HELLO_C_BYTES}); did --gc-sections stop working?",
    );

    println!("cargo:rustc-env=STAROS_HELLO_C_IMAGE={}", elf.display());
}

/// Find a program on `PATH`.
fn which(program: &str) -> Result<PathBuf, ()> {
    let path = env::var_os("PATH").ok_or(())?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or(())
}

/// Find a tool that ships inside the Rust toolchain's `rustlib` bin directory.
fn llvm_tool(name: &str) -> Option<PathBuf> {
    let sysroot = Command::new(env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--print")
        .arg("sysroot")
        .output()
        .ok()?;
    let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
    let entries = std::fs::read_dir(Path::new(&sysroot).join("lib/rustlib")).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("bin").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Flags every EL0 service is built with.
///
/// `-Cforce-frame-pointers=yes` is not an optimization setting but a debugging
/// contract: the kernel's fault handler walks the `x29` chain to print a backtrace,
/// and a program compiled without frame pointers gives it one frame and then
/// garbage. It costs a register and a couple of instructions per call, which is the
/// cheapest debugging tool in this tree.
/// `-Cdebuginfo=2` costs nothing in the image — the copy the kernel embeds is
/// stripped — and buys gdb the call-frame information it needs to unwind. Without
/// it gdb has symbols but no CFI, falls back on guessing from the prologue, and
/// reports a caller frame that belongs to the kernel: a wrong answer that looks
/// like a right one.
const SERVICE_FLAGS: &[&str] =
    &["-Copt-level=2", "-Cpanic=abort", "-Cforce-frame-pointers=yes", "-Cdebuginfo=2"];

/// The `llvm-objcopy` that ships with the Rust toolchain, or a cross binutils if
/// someone has one. Same search as `scripts/qemu-run.sh`, for the same reason:
/// no extra dependency to install.
fn llvm_objcopy() -> PathBuf {
    llvm_tool("llvm-objcopy").unwrap_or_else(|| PathBuf::from("llvm-objcopy"))
}

/// Strip `debug_elf` into `elf`, and fail the build if the tool is missing rather
/// than embedding an unstripped image and wondering later why the kernel grew.
fn strip_to(objcopy: &Path, debug_elf: &Path, elf: &Path) {
    let status = Command::new(objcopy)
        .arg("--strip-all")
        .arg(debug_elf)
        .arg(elf)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", objcopy.display()));
    assert!(status.success(), "{} failed on {}", objcopy.display(), debug_elf.display());
}

/// Build the `fssrv` EL0 program and publish its ELF path.
///
/// Links against the `cpio` rlib the device manager's build already produced: the
/// archive parser belongs in exactly one place, host-tested, and a server that
/// re-implemented the header arithmetic would be the second place for the same bug
/// to live.
fn build_fssrv(
    manifest_dir: &str,
    out_dir: &str,
    rustc: &str,
    image_ld: &Path,
    objcopy: &Path,
    cpio_rlib: &Path,
    abi_rlib: &Path,
) {
    let src = canonical(&Path::new(manifest_dir).join("../../services/fssrv/main.rs"));
    println!("cargo:rerun-if-changed={}", src.display());

    let debug_elf = Path::new(out_dir).join("fssrv.debug.elf");
    let elf = Path::new(out_dir).join("fssrv.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_fssrv"])
        .args(["--crate-type", "bin"])
        .args(SERVICE_FLAGS)
        .arg("--extern")
        .arg(format!("staros_cpio={}", cpio_rlib.display()))
        .arg("--extern")
        .arg(format!("staros_abi={}", abi_rlib.display()))
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&debug_elf)
        .arg(&src)
        .status()
        .expect("failed to spawn rustc for fssrv");
    assert!(status.success(), "rustc failed to build fssrv");
    strip_to(objcopy, &debug_elf, &elf);

    const MAX_FSSRV_BYTES: u64 = 128 * 1024;
    let size = std::fs::metadata(&elf)
        .expect("fssrv image was not produced")
        .len();
    assert!(
        size <= MAX_FSSRV_BYTES,
        "fssrv image is {size} bytes (> {MAX_FSSRV_BYTES}); the layout regressed",
    );

    println!("cargo:rustc-env=STAROS_FSSRV_IMAGE={}", elf.display());
}

/// Build the `fsclient` EL0 program and publish its ELF path.
///
/// One `rustc` step and no crate of ours: the whole program is syscalls and a
/// message protocol, which is the point — a client of the file server needs
/// nothing that knows what a CPIO archive is.
fn build_fsclient(manifest_dir: &str, out_dir: &str, rustc: &str, image_ld: &Path, objcopy: &Path) {
    let src = canonical(&Path::new(manifest_dir).join("../../services/fsclient/main.rs"));
    println!("cargo:rerun-if-changed={}", src.display());

    let debug_elf = Path::new(out_dir).join("fsclient.debug.elf");
    let elf = Path::new(out_dir).join("fsclient.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_fsclient"])
        .args(["--crate-type", "bin"])
        .args(SERVICE_FLAGS)
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&debug_elf)
        .arg(&src)
        .status()
        .expect("failed to spawn rustc for fsclient");
    assert!(status.success(), "rustc failed to build fsclient");
    strip_to(objcopy, &debug_elf, &elf);

    const MAX_FSCLIENT_BYTES: u64 = 64 * 1024;
    let size = std::fs::metadata(&elf)
        .expect("fsclient image was not produced")
        .len();
    assert!(
        size <= MAX_FSCLIENT_BYTES,
        "fsclient image is {size} bytes (> {MAX_FSCLIENT_BYTES}); the layout regressed",
    );

    println!("cargo:rustc-env=STAROS_FSCLIENT_IMAGE={}", elf.display());
}

/// Build the `inputsrv` EL0 program and publish its ELF path.
///
/// Two steps, like `devicemgr`: the `virtio` crate to an rlib, then the driver
/// against it. The layout arithmetic it needs is the *whole* reason that crate
/// exists — a driver that computed its own ring offsets would be the one place
/// the mistake could not be host-tested.
fn build_inputsrv(manifest_dir: &str, out_dir: &str, rustc: &str, image_ld: &Path, objcopy: &Path) {
    let virtio_src = canonical(&Path::new(manifest_dir).join("../virtio/src/lib.rs"));
    let src = canonical(&Path::new(manifest_dir).join("../../services/inputsrv/main.rs"));
    println!("cargo:rerun-if-changed={}", virtio_src.display());
    println!("cargo:rerun-if-changed={}", src.display());

    let virtio_rlib = Path::new(out_dir).join("libstaros_virtio.rlib");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_virtio"])
        .args(["--crate-type", "lib"])
        .arg("-Copt-level=2")
        .arg("-Cpanic=abort")
        .arg("-o")
        .arg(&virtio_rlib)
        .arg(&virtio_src)
        .status()
        .expect("failed to spawn rustc for the virtio rlib");
    assert!(status.success(), "rustc failed to build the virtio rlib");

    let debug_elf = Path::new(out_dir).join("inputsrv.debug.elf");
    let elf = Path::new(out_dir).join("inputsrv.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_inputsrv"])
        .args(["--crate-type", "bin"])
        .args(SERVICE_FLAGS)
        .arg("--extern")
        .arg(format!("staros_virtio={}", virtio_rlib.display()))
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&debug_elf)
        .arg(&src)
        .status()
        .expect("failed to spawn rustc for inputsrv");
    assert!(status.success(), "rustc failed to build inputsrv");
    strip_to(objcopy, &debug_elf, &elf);

    const MAX_INPUTSRV_BYTES: u64 = 64 * 1024;
    let size = std::fs::metadata(&elf)
        .expect("inputsrv image was not produced")
        .len();
    assert!(
        size <= MAX_INPUTSRV_BYTES,
        "inputsrv image is {size} bytes (> {MAX_INPUTSRV_BYTES}); the layout regressed",
    );

    println!("cargo:rustc-env=STAROS_INPUTSRV_IMAGE={}", elf.display());
}

/// Build the `displaysrv` EL0 program and publish its ELF path.
///
/// One `rustc` step: unlike `devicemgr` it links against no crate of ours. It owns
/// the screen and speaks a message protocol, and both of those are plain
/// arithmetic over slices the kernel already handed it.
fn build_displaysrv(
    manifest_dir: &str,
    out_dir: &str,
    rustc: &str,
    image_ld: &Path,
    objcopy: &Path,
) {
    let src = canonical(&Path::new(manifest_dir).join("../../services/displaysrv/main.rs"));
    println!("cargo:rerun-if-changed={}", src.display());

    let debug_elf = Path::new(out_dir).join("displaysrv.debug.elf");
    let elf = Path::new(out_dir).join("displaysrv.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_displaysrv"])
        .args(["--crate-type", "bin"])
        .args(SERVICE_FLAGS)
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&debug_elf)
        .arg(&src)
        .status()
        .expect("failed to spawn rustc for displaysrv");
    assert!(status.success(), "rustc failed to build displaysrv");
    strip_to(objcopy, &debug_elf, &elf);

    const MAX_DISPLAYSRV_BYTES: u64 = 64 * 1024;
    let size = std::fs::metadata(&elf)
        .expect("displaysrv image was not produced")
        .len();
    assert!(
        size <= MAX_DISPLAYSRV_BYTES,
        "displaysrv image is {size} bytes (> {MAX_DISPLAYSRV_BYTES}); the layout regressed",
    );

    println!("cargo:rustc-env=STAROS_DISPLAYSRV_IMAGE={}", elf.display());
}

/// Build the `devicemgr` EL0 program and publish its ELF path.
///
/// Unlike `init`, this program links against the `fdt` crate, so the build is two
/// `rustc` steps: compile `fdt` to an rlib for the bare-metal target, then compile
/// `devicemgr` against it with `--extern`. Still no `-Zbuild-std`: at
/// `-Copt-level=2` the compiler inlines `fdt`'s slice work, so `core`'s memory
/// intrinsics are never referenced and the pre-compiled `core` is enough.
/// Returns the path to the `cpio` rlib it built, which the file server links
/// against too.
fn build_devicemgr(
    manifest_dir: &str,
    out_dir: &str,
    rustc: &str,
    image_ld: &Path,
    objcopy: &Path,
) -> PathBuf {
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
    // Stripping matters most here: unstripped, the `fdt`/`cpio` symbol data balloons
    // the ELF to hundreds of KiB the kernel would `include_bytes!` verbatim. The
    // loadable segments are unaffected — the kernel maps those, not the symbol
    // tables — so the unstripped copy beside it costs nothing at runtime and is
    // what turns a backtrace back into names.
    let dm_debug_elf = Path::new(out_dir).join("devicemgr.debug.elf");
    let dm_elf = Path::new(out_dir).join("devicemgr.elf");
    let status = Command::new(rustc)
        .args(["--edition", "2021"])
        .args(["--target", "aarch64-unknown-none"])
        .args(["--crate-name", "staros_devicemgr"])
        .args(["--crate-type", "bin"])
        .args(SERVICE_FLAGS)
        .arg("--extern")
        .arg(format!("staros_fdt={}", fdt_rlib.display()))
        .arg("--extern")
        .arg(format!("staros_cpio={}", cpio_rlib.display()))
        .arg(format!("-Clink-arg=-T{}", image_ld.display()))
        .arg("-Clink-arg=-z")
        .arg("-Clink-arg=max-page-size=4096")
        .arg("-o")
        .arg(&dm_debug_elf)
        .arg(&dm_src)
        .status()
        .expect("failed to spawn rustc for devicemgr");
    assert!(status.success(), "rustc failed to build devicemgr");
    strip_to(objcopy, &dm_debug_elf, &dm_elf);

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
    cpio_rlib
}

/// Canonicalize a path, panicking with a clear message if it does not exist —
/// used for the init sources so a mislaid file fails the build immediately rather
/// than silently dropping a `rerun-if-changed` dependency.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("init image source {} not found: {e}", path.display()))
}
