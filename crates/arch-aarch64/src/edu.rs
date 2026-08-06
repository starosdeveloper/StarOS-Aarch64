//! QEMU `edu` device — a tiny PCIe DMA engine used to prove the SMMU works.
//!
//! `edu` (QEMU's teaching device, `-device edu`) sits on the PCIe bus and can master
//! it: told a RAM address, a device-buffer offset, and a length, it copies between
//! guest RAM and its own 4 KiB internal buffer. Behind an `iommu=smmuv3` root complex
//! that RAM address is an **IOVA the SMMU translates** — which is exactly what lets us
//! show, end to end, that a real bus master reaches only the pages we mapped for its
//! StreamID and is aborted on any other address. This module is the MMIO half; the
//! kernel drives the SMMU binding and checks the result.
//!
//! Register and semantics are from QEMU `hw/misc/edu.c`.

use core::ptr::{read_volatile, write_volatile};

use crate::mmu;

/// The identification/liveness register (`BAR0 + 0x00`) reads back this constant on
/// a live `edu` — a cheap "is the device really there and decoding MMIO" check.
pub const ID_MAGIC: u32 = 0x0100_00ed;

/// Base of the device's internal 4 KiB DMA buffer, in *device* address space. DMA
/// addresses on the device side are given relative to this.
pub const DEV_BUF_BASE: u64 = 0x4_0000;
/// Size of that internal buffer — the most a single DMA can move.
pub const DEV_BUF_SIZE: u64 = 0x1000;

const REG_ID: usize = 0x00;
const REG_DMA_SRC: usize = 0x80;
const REG_DMA_DST: usize = 0x88;
const REG_DMA_CNT: usize = 0x90;
const REG_DMA_CMD: usize = 0x98;

const DMA_RUN: u64 = 1 << 0;
/// Direction bit: set = device buffer → RAM; clear = RAM → device buffer.
const DMA_TO_RAM: u64 = 1 << 1;

/// A bounded wait so a wedged transfer fails instead of hanging boot.
const SPIN_LIMIT: u32 = 1 << 24;

/// A mapped `edu` device.
pub struct Edu {
    base: usize,
}

impl Edu {
    /// # Safety
    /// `bar0_phys` must be the device's assigned BAR0 base, mapped Device in the
    /// linear map (the 32-bit PCI MMIO window is, via [`mmu`]'s below-RAM device map).
    #[must_use]
    pub unsafe fn new(bar0_phys: u64) -> Self {
        Self {
            base: mmu::phys_to_virt(bar0_phys) as usize,
        }
    }

    /// Read the identification register — should equal [`ID_MAGIC`].
    #[must_use]
    pub fn liveness(&self) -> u32 {
        // SAFETY: `base + REG_ID` is the device's MMIO id register.
        unsafe { read_volatile((self.base + REG_ID) as *const u32) }
    }

    /// Spin until the DMA engine clears its RUN bit, or the bound is hit.
    fn wait_dma(&self) -> bool {
        for _ in 0..SPIN_LIMIT {
            // SAFETY: reading the DMA command register has no side effects.
            let cmd = unsafe { read_volatile((self.base + REG_DMA_CMD) as *const u64) };
            if cmd & DMA_RUN == 0 {
                return true;
            }
        }
        false
    }

    /// Program a transfer and start it. `ram_iova` is the RAM-side address (an IOVA,
    /// SMMU-translated); `dev_off` an offset into the device buffer; `to_ram` picks
    /// the direction. Returns whether the engine completed within the spin bound.
    ///
    /// # Safety
    /// The device is mapped and owns its registers for this call.
    unsafe fn run(&self, ram_iova: u64, dev_off: u64, len: u64, to_ram: bool) -> bool {
        let dev_addr = DEV_BUF_BASE + dev_off;
        let (src, dst) = if to_ram {
            (dev_addr, ram_iova) // device buffer → RAM
        } else {
            (ram_iova, dev_addr) // RAM → device buffer
        };
        // SAFETY: MMIO writes to the DMA engine registers, then a bounded poll.
        unsafe {
            write_volatile((self.base + REG_DMA_SRC) as *mut u64, src);
            write_volatile((self.base + REG_DMA_DST) as *mut u64, dst);
            write_volatile((self.base + REG_DMA_CNT) as *mut u64, len);
            let cmd = DMA_RUN | if to_ram { DMA_TO_RAM } else { 0 };
            write_volatile((self.base + REG_DMA_CMD) as *mut u64, cmd);
        }
        self.wait_dma()
    }

    /// DMA `len` bytes from RAM at `ram_iova` into the device buffer at `dev_off`.
    ///
    /// # Safety
    /// As [`Edu::new`]; `len <= DEV_BUF_SIZE - dev_off`.
    pub unsafe fn read_from_ram(&self, ram_iova: u64, dev_off: u64, len: u64) -> bool {
        // SAFETY: forwarded.
        unsafe { self.run(ram_iova, dev_off, len, false) }
    }

    /// DMA `len` bytes from the device buffer at `dev_off` out to RAM at `ram_iova`.
    ///
    /// # Safety
    /// As [`Edu::new`]; `len <= DEV_BUF_SIZE - dev_off`.
    pub unsafe fn write_to_ram(&self, dev_off: u64, ram_iova: u64, len: u64) -> bool {
        // SAFETY: forwarded.
        unsafe { self.run(ram_iova, dev_off, len, true) }
    }
}
