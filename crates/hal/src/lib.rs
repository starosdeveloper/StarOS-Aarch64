//! Hardware Abstraction Layer — trait definitions only.
//!
//! The HAL is where the kernel says *what* it needs from hardware without
//! saying *how* any particular chip provides it. Arch crates and drivers
//! implement these traits; the portable kernel core depends only on the traits.
//! This inversion is what lets the core be unit-tested on the host and keeps
//! chip-specific `unsafe` MMIO out of the scheduler.
#![no_std]

use staros_abi::error::KResult;

/// A byte-oriented serial console (UART, semihosting, virtio-console, ...).
///
/// Used for early boot diagnostics before any richer I/O stack exists.
pub trait SerialConsole {
    /// Write a single byte, blocking until the hardware accepts it.
    fn write_byte(&self, byte: u8);

    /// Write a string as UTF-8 bytes. Provided in terms of [`write_byte`].
    ///
    /// [`write_byte`]: SerialConsole::write_byte
    fn write_str(&self, s: &str) {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }
}

/// A monotonic time source, expressed in raw hardware ticks.
pub trait Timer {
    /// Current tick count. Guaranteed non-decreasing.
    fn now_ticks(&self) -> u64;

    /// Frequency of the tick counter in Hz, used to convert ticks to wall time.
    fn frequency_hz(&self) -> u64;
}

/// An interrupt controller (e.g. an ARM GIC).
pub trait InterruptController {
    /// Enable delivery of the given interrupt id to the current CPU.
    ///
    /// # Errors
    /// Returns an error if `irq` is out of range for this controller.
    fn enable(&self, irq: u32) -> KResult<()>;

    /// Disable delivery of the given interrupt id.
    ///
    /// # Errors
    /// Returns an error if `irq` is out of range for this controller.
    fn disable(&self, irq: u32) -> KResult<()>;

    /// Acknowledge the highest-priority pending interrupt, returning its id.
    fn acknowledge(&self) -> Option<u32>;

    /// Signal end-of-interrupt for a previously acknowledged id.
    fn end_of_interrupt(&self, irq: u32);
}
