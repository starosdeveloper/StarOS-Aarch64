//! STAR OS Application Binary Interface.
//!
//! This crate is the single source of truth for the contract between the
//! microkernel and everything outside it (user-space services, drivers,
//! tooling). It is deliberately tiny, dependency-free and `no_std`: both sides
//! of a syscall link against *the same* definitions here instead of
//! re-declaring magic numbers.
//!
//! `no_std` except under `cargo test`, where the host test harness needs `std`.
#![cfg_attr(not(test), no_std)]

pub mod affinity;
pub mod error;
pub mod fsproto;
pub mod syscall;

/// An opaque, unforgeable reference to a kernel object (a capability handle).
///
/// User space never sees raw kernel pointers; it names objects — memory
/// regions, endpoints, tasks — through these handles. The kernel resolves a
/// handle against the calling task's capability space on every syscall.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Handle(pub u32);

impl Handle {
    /// The reserved null handle, valid in no capability space.
    pub const NULL: Handle = Handle(0);

    /// Returns `true` if this is the reserved null handle.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == Handle::NULL.0
    }
}
