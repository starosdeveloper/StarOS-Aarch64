//! PL011 UART driver for the QEMU `virt` machine.
//!
//! The `virt` machine wires a PrimeCell PL011 at `0x0900_0000`. For early boot
//! we only need transmit, and QEMU accepts bytes without any clock/baud setup,
//! so this driver is intentionally the smallest thing that can print.

use core::fmt;
use core::ptr::write_volatile;

use staros_hal::SerialConsole;

use crate::mmu;

/// **Physical** MMIO base of the PL011 on QEMU `virt` — the address the device
/// tree reports, not the one the kernel dereferences.
pub const PL011_BASE: usize = 0x0900_0000;

/// A PL011 UART at a fixed MMIO base address.
pub struct Pl011 {
    /// Where the registers are *reached*: the base's linear-map address, resolved
    /// once in the constructor so the hot path is a plain store.
    base: usize,
}

impl Pl011 {
    /// The data register offset (`UARTDR`): writing a byte transmits it.
    const DR: usize = 0x00;

    /// Construct a driver for the PL011 whose registers are at **physical**
    /// `base`. The translation to the address this actually writes to is done
    /// here: the kernel's virtual memory layout is the arch crate's business, so
    /// callers pass the number the device tree gave them and nothing else.
    ///
    /// # Safety
    /// `base` must be the physical MMIO base of a real PL011, covered by the
    /// kernel's linear map (every address below [`mmu::LINEAR_MAP_LIMIT`] is, as
    /// Device memory), and owned exclusively by the caller; concurrent writers
    /// would race on `UARTDR`.
    #[must_use]
    pub const unsafe fn new(base: usize) -> Self {
        Self {
            base: mmu::phys_to_virt(base as u64) as usize,
        }
    }

    /// Construct a driver for the standard QEMU `virt` PL011.
    ///
    /// # Safety
    /// Same contract as [`Pl011::new`]; assumes the QEMU `virt` memory map.
    #[must_use]
    pub const unsafe fn qemu_virt() -> Self {
        // SAFETY: forwarded to the caller — `PL011_BASE` is correct for `virt`.
        unsafe { Self::new(PL011_BASE) }
    }
}

/// Lets `write!`/`writeln!` target the console, so callers format with the
/// standard machinery instead of hand-rolling hex/decimal conversion.
impl fmt::Write for Pl011 {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        SerialConsole::write_str(self, s);
        Ok(())
    }
}

impl SerialConsole for Pl011 {
    fn write_byte(&self, byte: u8) {
        // SAFETY: `base + DR` is the linear-map address of the transmit register
        // of a PL011 owned by this instance (see the constructor's safety
        // contract). A byte write is the documented way to transmit; QEMU never
        // stalls, so we don't poll TXFF.
        unsafe {
            write_volatile((self.base + Self::DR) as *mut u32, u32::from(byte));
        }
    }
}
