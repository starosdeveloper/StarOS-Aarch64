//! QEMU `ramfb` framebuffer, configured over the fw_cfg DMA interface.
//!
//! This is a *test vehicle*, not a device we ship. On real hardware the first
//! framebuffer comes from firmware (a Raspberry Pi's VideoCore mailbox, a laptop's
//! GOP), and that will be a different arch path producing the same
//! [`FramebufferInfo`]. But on the QEMU `virt` machine there is no firmware
//! framebuffer at all — so to exercise the portable console (`staros_framebuffer`)
//! against a *real scanout engine* before any board exists, we use `ramfb`: the
//! guest allocates a buffer in its own RAM and tells QEMU, via fw_cfg, to scan it
//! out. Add `-device ramfb` and a display, and whatever the console draws appears
//! on screen — which is what makes the pixel path falsifiable in CI (screendump)
//! rather than only unit-tested.
//!
//! fw_cfg's DMA interface is a tiny big-endian MMIO protocol: write a `DmaAccess`
//! descriptor into guest RAM, then poke its physical address into the DMA
//! register; QEMU performs the transfer synchronously and clears the control word.
//! We use it twice — once to read the file directory (to find the `etc/ramfb`
//! key), once to write the framebuffer configuration to that key.

use core::ptr::write_volatile;

use staros_mm::{PhysAddr, PAGE_SIZE};

use crate::mmu;

/// fw_cfg MMIO register offsets (the `qemu,fw-cfg-mmio` layout).
const REG_DMA_HI: usize = 0x10;
const REG_DMA_LO: usize = 0x14;

/// Selector of the fw_cfg file directory (`FW_CFG_FILE_DIR`).
const FILE_DIR: u16 = 0x0019;

/// `DmaAccess.control` bits (big-endian in memory, native here).
const CTL_ERROR: u32 = 0x01;
const CTL_READ: u32 = 0x02;
const CTL_SELECT: u32 = 0x08;
const CTL_WRITE: u32 = 0x10;

/// `DRM_FORMAT_XRGB8888` — `fourcc_code('X','R','2','4')`. In memory each pixel is
/// little-endian `B,G,R,X`, which is exactly what `PixelFormat::xrgb8888` writes.
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// Resolution we ask for. Deliberately modest so the buffer fits a 128 MiB guest
/// and the whole thing stays a cheap smoke test.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

// The framebuffer contract shared with every other source (mailbox, GOP, …).
pub use crate::fbinfo::FramebufferInfo;

/// Configure `ramfb` at fw_cfg base `fw_cfg_phys`, allocating the pixel buffer via
/// `alloc`. Returns the framebuffer geometry, or `None` if this machine has no
/// `etc/ramfb` file (i.e. `-device ramfb` was not supplied) or memory ran out.
///
/// # Safety
/// `fw_cfg_phys` must be the MMIO base of a QEMU `qemu,fw-cfg-mmio` block, covered
/// by the linear map, and this must run single-threaded during early boot (it owns
/// the fw_cfg registers for the duration). The returned buffer's frames are never
/// freed — the caller must account for them as permanently reserved.
pub unsafe fn init(
    fw_cfg_phys: u64,
    mut alloc: impl FnMut(usize) -> Option<PhysAddr>,
) -> Option<FramebufferInfo> {
    let base = mmu::phys_to_virt(fw_cfg_phys) as usize;

    // A single scratch page holds the DMA descriptor, the directory read buffer,
    // and the ramfb config — all reachable by QEMU at known physical offsets.
    let scratch = alloc(1)?;
    let scratch_phys = scratch.0 as u64;
    let scratch_va = mmu::phys_to_virt(scratch_phys) as *mut u8;
    // SAFETY: a freshly allocated, linear-mapped page we own exclusively.
    unsafe { core::ptr::write_bytes(scratch_va, 0, PAGE_SIZE) };

    // The DmaAccess descriptor lives at the start of the scratch page
    // (`scratch_phys`/`scratch_va`); the directory buffer and config sit after it.
    const DIR_OFF: usize = 64; // directory read buffer
    const DIR_CAP: usize = 2048; // enough for QEMU virt's ~20 files
    const CFG_OFF: usize = 3072; // ramfb config (28 bytes)

    // --- find the etc/ramfb selector by reading the file directory ------------
    // SAFETY: descriptor and buffer are in the scratch page; `base` is fw_cfg.
    let ok = unsafe {
        dma(
            base,
            scratch_phys,
            scratch_va,
            u32::from(FILE_DIR) << 16 | CTL_SELECT | CTL_READ,
            DIR_CAP as u32,
            scratch_phys + DIR_OFF as u64,
        )
    };
    if !ok {
        return None;
    }

    // Directory layout: BE u32 count, then `count` × 64-byte entries of
    // { BE u32 size; BE u16 select; u16 reserved; char name[56] }.
    // SAFETY: the read filled up to DIR_CAP bytes of the scratch page.
    let dir = unsafe { core::slice::from_raw_parts(scratch_va.add(DIR_OFF), DIR_CAP) };
    let count = u32::from_be_bytes([dir[0], dir[1], dir[2], dir[3]]) as usize;
    let mut ramfb_key: Option<u16> = None;
    for i in 0..count {
        let off = 4 + i * 64;
        if off + 64 > DIR_CAP {
            break;
        }
        let entry = &dir[off..off + 64];
        let select = u16::from_be_bytes([entry[4], entry[5]]);
        let name = &entry[8..64];
        if name_is(name, b"etc/ramfb") {
            ramfb_key = Some(select);
            break;
        }
    }
    let key = ramfb_key?;

    // --- allocate and clear the pixel buffer ----------------------------------
    let stride = WIDTH as usize * 4;
    let fb_bytes = stride * HEIGHT as usize;
    let fb_pages = fb_bytes.div_ceil(PAGE_SIZE);
    let fb = alloc(fb_pages)?;
    let fb_phys = fb.0 as u64;
    let fb_va = mmu::phys_to_virt(fb_phys) as *mut u8;
    // SAFETY: freshly allocated, linear-mapped, owned; sized for the buffer.
    unsafe { core::ptr::write_bytes(fb_va, 0, fb_pages * PAGE_SIZE) };

    // --- write the ramfb config (all fields big-endian) -----------------------
    // struct { u64 addr; u32 fourcc; u32 flags; u32 width; u32 height; u32 stride }
    // SAFETY: writing into our scratch page.
    unsafe {
        let cfg = scratch_va.add(CFG_OFF);
        write_be64(cfg.add(0), fb_phys);
        write_be32(cfg.add(8), DRM_FORMAT_XRGB8888);
        write_be32(cfg.add(12), 0); // flags
        write_be32(cfg.add(16), WIDTH);
        write_be32(cfg.add(20), HEIGHT);
        write_be32(cfg.add(24), stride as u32);
    }

    // SAFETY: descriptor and config are in the scratch page; `base` is fw_cfg.
    let ok = unsafe {
        dma(
            base,
            scratch_phys,
            scratch_va,
            u32::from(key) << 16 | CTL_SELECT | CTL_WRITE,
            28,
            scratch_phys + CFG_OFF as u64,
        )
    };
    if !ok {
        return None;
    }

    Some(FramebufferInfo {
        phys: fb_phys,
        width: WIDTH as usize,
        height: HEIGHT as usize,
        stride,
    })
}

/// Run one fw_cfg DMA transfer and wait for completion. Returns `false` if QEMU
/// reports the error bit. `desc_va`/`desc_phys` point at a 16-byte scratch region
/// used for the `DmaAccess` descriptor.
///
/// # Safety
/// `base` must be the linear-map address of the fw_cfg MMIO block; the descriptor
/// and the transfer target must be linear-mapped scratch we own.
unsafe fn dma(
    base: usize,
    desc_phys: u64,
    desc_va: *mut u8,
    control: u32,
    length: u32,
    target_phys: u64,
) -> bool {
    // SAFETY: writing the DmaAccess descriptor into our own scratch.
    unsafe {
        write_be32(desc_va.add(0), control);
        write_be32(desc_va.add(4), length);
        write_be64(desc_va.add(8), target_phys);
    }
    // Ensure the descriptor is in memory before QEMU reads it.
    // SAFETY: a barrier has no preconditions.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };

    // The DMA register is a 64-bit big-endian register; writing the low half
    // triggers the (synchronous, under TCG) transfer. Store each half so that a
    // big-endian read reconstructs the address.
    let hi = (desc_phys >> 32) as u32;
    let lo = desc_phys as u32;
    // SAFETY: `base + REG_DMA_*` are the fw_cfg DMA register halves.
    unsafe {
        write_volatile((base + REG_DMA_HI) as *mut u32, hi.to_be());
        write_volatile((base + REG_DMA_LO) as *mut u32, lo.to_be());
    }

    // Poll the control word: QEMU clears it to 0 on success, or sets the error
    // bit. Bounded so a broken machine cannot hang boot forever.
    for _ in 0..1_000_000 {
        // SAFETY: reading back our own descriptor's control field.
        let ctl = unsafe { read_be32(desc_va.add(0)) };
        if ctl & CTL_ERROR != 0 {
            return false;
        }
        if ctl == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// True if the NUL-terminated `name` field equals `want`.
fn name_is(name: &[u8], want: &[u8]) -> bool {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    &name[..end] == want
}

/// # Safety
/// `p` must point at four writable bytes.
unsafe fn write_be32(p: *mut u8, v: u32) {
    // SAFETY: caller guarantees four writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(v.to_be_bytes().as_ptr(), p, 4) };
}

/// # Safety
/// `p` must point at eight writable bytes.
unsafe fn write_be64(p: *mut u8, v: u64) {
    // SAFETY: caller guarantees eight writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(v.to_be_bytes().as_ptr(), p, 8) };
}

/// # Safety
/// `p` must point at four readable bytes.
unsafe fn read_be32(p: *const u8) -> u32 {
    let mut b = [0u8; 4];
    // SAFETY: caller guarantees four readable bytes.
    unsafe { core::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), 4) };
    u32::from_be_bytes(b)
}
