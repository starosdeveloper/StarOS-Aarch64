//! Inter-process communication primitives.
//!
//! In a microkernel almost everything — drivers, filesystems, the network
//! stack — is a user-space service reached by message passing. IPC is therefore
//! a hot path and a security boundary, which is why the message format is a
//! fixed-size, copyable value type rather than an ad-hoc buffer.
//!
//! `no_std` except under `cargo test`, where the host test harness needs `std`.
#![cfg_attr(not(test), no_std)]

use staros_abi::Handle;

/// The number of inline data words carried by a [`Message`].
pub const MESSAGE_WORDS: usize = 4;

/// A fixed-size IPC message.
///
/// Small messages travel entirely in registers/inline words; bulk data is
/// transferred out-of-band by granting a memory capability referenced through
/// [`Message::cap`]. Keeping the struct `Copy` and fixed-size means the kernel
/// never allocates on the send path.
///
/// `#[repr(C)]` because both the kernel and user space read and write this
/// struct through a shared buffer pointer on the `Send`/`Recv` path — the layout
/// is part of the ABI and must not depend on the Rust field-ordering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Message {
    /// Caller-defined tag identifying the request (e.g. a method id).
    pub tag: u64,
    /// Inline payload words.
    pub words: [u64; MESSAGE_WORDS],
    /// An optional capability transferred alongside the message.
    pub cap: Handle,
}

impl Message {
    /// Construct a tagged message with no inline data and no capability.
    #[must_use]
    pub const fn new(tag: u64) -> Self {
        Self {
            tag,
            words: [0; MESSAGE_WORDS],
            cap: Handle::NULL,
        }
    }

    /// Returns `true` if this message carries a capability.
    #[must_use]
    pub const fn transfers_cap(&self) -> bool {
        !self.cap.is_null()
    }
}

/// A rendezvous point that tasks send to and receive from, named by a
/// [`Handle`] in user space.
///
/// The queueing/blocking policy is intentionally left unimplemented here — this
/// crate defines the *shape* of an endpoint; the kernel scheduler wires up the
/// wait queues.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    /// Stable identifier of this endpoint within the kernel object table.
    pub id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_message_carries_no_cap() {
        let m = Message::new(7);
        assert_eq!(m.tag, 7);
        assert!(!m.transfers_cap());
        assert_eq!(m.words, [0; MESSAGE_WORDS]);
    }
}
