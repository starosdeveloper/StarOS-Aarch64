//! Minimal PCI Express **ECAM** access — just enough to find one device, give it an
//! MMIO window, and let it master the bus.
//!
//! This is not a PCI stack: no bridges, no capability walk, no interrupt routing. It
//! exists for the SMMU end-to-end test, where we need a *real bus-master* behind the
//! IOMMU to prove translation actually happens — QEMU's teaching device `edu` fills
//! that role. Enhanced Configuration Access maps every function's 4 KiB config space
//! flat in MMIO: `ecam_base + (bus<<20) + (dev<<15) + (func<<12) + reg`. The tree
//! gives us the ECAM base and the MMIO window a BAR may live in.

use core::ptr::{read_volatile, write_volatile};

use crate::mmu;

/// A PCI bus/device/function address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    /// Bus number.
    pub bus: u8,
    /// Device (slot) number.
    pub dev: u8,
    /// Function number.
    pub func: u8,
}

impl Bdf {
    /// The RequesterID the PCIe root complex emits for this function, which QEMU's
    /// `virt` maps 1:1 to an SMMU **StreamID** (`iommu-map` is the identity range).
    #[must_use]
    pub fn stream_id(self) -> u32 {
        (u32::from(self.bus) << 8) | (u32::from(self.dev) << 3) | u32::from(self.func)
    }
}

/// A device we found, gave a BAR, and enabled for bus mastering.
#[derive(Clone, Copy, Debug)]
pub struct Device {
    /// Where it sits on the bus (and thus its StreamID).
    pub bdf: Bdf,
    /// Physical base of its BAR0 MMIO region, as we assigned it.
    pub bar0: u64,
}

// Config-space register offsets.
const CFG_VENDOR: usize = 0x00; // u16 vendor then u16 device
const CFG_COMMAND: usize = 0x04; // u16
const CFG_BAR0: usize = 0x10; // u32
const CMD_MEM_SPACE: u16 = 1 << 1;
const CMD_BUS_MASTER: u16 = 1 << 2;

/// `0xFFFF` vendor id = no function present in this slot.
const VENDOR_NONE: u16 = 0xFFFF;

/// An ECAM region mapped into the kernel linear map.
struct Ecam {
    base: usize,
}

impl Ecam {
    /// # Safety
    /// `ecam_phys` must be the machine's ECAM base and already mapped Device in the
    /// linear map (see [`mmu::map_device_block`]).
    unsafe fn new(ecam_phys: u64) -> Self {
        Self {
            base: mmu::phys_to_virt(ecam_phys) as usize,
        }
    }

    fn ptr(&self, bdf: Bdf, off: usize) -> usize {
        self.base
            + (usize::from(bdf.bus) << 20)
            + (usize::from(bdf.dev) << 15)
            + (usize::from(bdf.func) << 12)
            + off
    }

    /// # Safety
    /// `bdf`/`off` must land in the mapped ECAM window.
    unsafe fn r32(&self, bdf: Bdf, off: usize) -> u32 {
        // SAFETY: config space is Device memory in the linear map; `off` is aligned.
        unsafe { read_volatile(self.ptr(bdf, off) as *const u32) }
    }
    /// # Safety: as [`Ecam::r32`].
    unsafe fn w32(&self, bdf: Bdf, off: usize, v: u32) {
        // SAFETY: as `r32`.
        unsafe { write_volatile(self.ptr(bdf, off) as *mut u32, v) }
    }
    /// # Safety: as [`Ecam::r32`].
    unsafe fn r16(&self, bdf: Bdf, off: usize) -> u16 {
        // SAFETY: as `r32`; config space allows 16-bit accesses.
        unsafe { read_volatile(self.ptr(bdf, off) as *const u16) }
    }
    /// # Safety: as [`Ecam::r32`].
    unsafe fn w16(&self, bdf: Bdf, off: usize, v: u16) {
        // SAFETY: as `r32`.
        unsafe { write_volatile(self.ptr(bdf, off) as *mut u16, v) }
    }
}

/// Find the first function on bus 0 whose (vendor, device) match, assign its BAR0 an
/// MMIO region inside `[mmio_base, mmio_base + mmio_len)`, and enable Memory-Space
/// decoding plus Bus-Master DMA. Returns the placed [`Device`], or `None` if the
/// device is absent or its BAR does not fit the window.
///
/// Only bus 0 is scanned (single-function, no bridges — all the test needs). BAR0 is
/// assumed to be a 32-bit memory BAR, which is what the `edu` device presents.
///
/// # Safety
/// `ecam_phys` must be the ECAM base, already Device-mapped. `[mmio_base, +mmio_len)`
/// must be a PCI MMIO window the host bridge decodes (from the tree's `ranges`) and
/// not in use by anything else. Runs single-threaded at EL1 during early boot.
pub unsafe fn find_and_enable(
    ecam_phys: u64,
    vendor: u16,
    device: u16,
    mmio_base: u64,
    mmio_len: u64,
) -> Option<Device> {
    // SAFETY: forwarded — ECAM base is mapped Device.
    let ecam = unsafe { Ecam::new(ecam_phys) };

    let mut bdf = None;
    for dev in 0..32u8 {
        let at = Bdf { bus: 0, dev, func: 0 };
        // SAFETY: within the mapped ECAM window.
        let id = unsafe { ecam.r32(at, CFG_VENDOR) };
        let (vid, did) = ((id & 0xFFFF) as u16, (id >> 16) as u16);
        if vid == VENDOR_NONE {
            continue;
        }
        if vid == vendor && did == device {
            bdf = Some(at);
            break;
        }
    }
    let bdf = bdf?;

    // Size the BAR: write all-ones, read the writable bits back. The size is the
    // magnitude of the lowest set bit after masking off the low type bits.
    // SAFETY: the device exists; probing BAR0 is the standard sizing dance.
    let bar0 = unsafe {
        let orig = ecam.r32(bdf, CFG_BAR0);
        ecam.w32(bdf, CFG_BAR0, 0xFFFF_FFFF);
        let probed = ecam.r32(bdf, CFG_BAR0);
        ecam.w32(bdf, CFG_BAR0, orig);
        probed
    };
    // Bit 0 = 0 means a memory BAR; bits [2:1] = 00 means 32-bit. Anything else is
    // outside what this minimal helper handles.
    if bar0 & 0b1 != 0 || bar0 & 0b110 != 0 {
        return None;
    }
    let size = (!(bar0 & 0xFFFF_FFF0)).wrapping_add(1) as u64;
    if size == 0 {
        return None;
    }
    // Place the BAR at the next `size`-aligned address in the window.
    let base = (mmio_base + size - 1) & !(size - 1);
    if base + size > mmio_base + mmio_len {
        return None;
    }

    // SAFETY: assign the BAR and turn on memory decoding + bus mastering so the
    // device can both be reached by MMIO and issue DMA.
    unsafe {
        ecam.w32(bdf, CFG_BAR0, base as u32);
        let cmd = ecam.r16(bdf, CFG_COMMAND);
        ecam.w16(bdf, CFG_COMMAND, cmd | CMD_MEM_SPACE | CMD_BUS_MASTER);
    }

    Some(Device { bdf, bar0: base })
}
