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

/// An interrupt controller: an ARM GIC, or an x86 APIC pair.
///
/// # What porting removed from this trait
///
/// It was written against the GIC and, until a second architecture arrived,
/// looked architecture-neutral. Two of its four methods were not.
///
/// [`AcknowledgingController::acknowledge`] used to live here. On a GIC it is
/// the operation that both *identifies* the pending interrupt and takes it: the
/// handler reads `GICC_IAR` and the register answers with an id. There is no
/// such register on x86 and no such question — the I/O APIC is programmed in
/// advance with "this line becomes vector 32", so by the time the handler runs
/// the CPU has already decided which one it is by choosing an IDT entry. A
/// method whose whole purpose is to ask the controller what happened cannot be
/// implemented by a controller that was told in advance. So it moved to a trait
/// of its own, which the GIC implements and the APIC does not.
///
/// [`InterruptController::end_of_interrupt`] kept its argument, and that was the
/// other choice available. The local APIC's EOI register takes only zero: the
/// controller clears the highest-priority in-service bit by itself and there is
/// nothing to name. The id stays in the signature because the GIC genuinely
/// needs it — `GICC_EOIR` is written with the id and misbehaves without it — and
/// because a caller that has just handled interrupt *n* always has *n* to hand.
/// It is documented as advisory rather than dropped, which is the honest way
/// round: an implementation that ignores it loses nothing, while one that needs
/// it and cannot have it is broken.
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

    /// Signal end-of-interrupt for the interrupt being handled.
    ///
    /// `irq` is **advisory**: controllers that need to be told which one to
    /// retire use it (the GIC's `GICC_EOIR`), and controllers that retire the
    /// highest-priority one by themselves ignore it (the local APIC's `EOI`,
    /// which accepts only zero). Callers pass the id they handled either way.
    fn end_of_interrupt(&self, irq: u32);
}

/// A controller that names the pending interrupt itself.
///
/// Separate from [`InterruptController`] because it is not a property every
/// controller has. A GIC is asked; an APIC has already answered by picking the
/// vector. Code that reads an id from the controller is therefore
/// architecture-specific by construction, and this trait is where that shows up
/// in the type system instead of in a comment.
pub trait AcknowledgingController: InterruptController {
    /// Acknowledge the highest-priority pending interrupt, returning its id.
    fn acknowledge(&self) -> Option<u32>;
}
