//! Driver framework.
//!
//! In this design drivers are ordinary crates that implement [`Driver`] and the
//! relevant [`staros_hal`] traits — they do not get privileged access to kernel
//! internals. This crate provides the common lifecycle they share. Concrete
//! drivers (a block device, a NIC, ...) will be added as sibling modules or
//! crates, each self-contained rather than woven through one megamodule.
#![no_std]

use staros_abi::error::KResult;

/// The lifecycle every driver implements.
///
/// `probe` decides whether the driver can handle the device it was offered;
/// `init` brings matched hardware into a usable state. Both are fallible so a
/// misbehaving driver reports an error instead of panicking the kernel.
pub trait Driver {
    /// Human-readable name, used in boot logs and diagnostics.
    fn name(&self) -> &str;

    /// Attempt to claim a device. Returns `Ok(true)` if this driver matches.
    ///
    /// # Errors
    /// Returns an error if probing touched hardware and that access failed.
    fn probe(&mut self) -> KResult<bool>;

    /// Bring the claimed device into service.
    ///
    /// # Errors
    /// Returns an error if the device could not be initialised.
    fn init(&mut self) -> KResult<()>;
}
