//! `init` — the first user-space process STAR OS starts.
//!
//! This crate holds the *portable, host-testable* part of `init`: the protocol
//! constants and message-building logic it shares with the rest of the system,
//! exercising the ABI ([`staros_abi`], [`staros_ipc`]) so it can't silently
//! regress. It is a `no_std` library (except under `cargo test`, where the host
//! harness needs `std`).
//!
//! The actual **loadable EL0 image** lives beside it in `boot/image.rs`: a
//! standalone, dependency-free program linked at `USER_BASE` by `boot/image.ld`.
//! It is *not* built by Cargo — the kernel's `build.rs` compiles it with `rustc`
//! and flattens it with `llvm-objcopy`, and the kernel loads the resulting
//! binary into a user address space at runtime (see the kernel's
//! `load_init_image`). Keeping the image separate from this library is what lets
//! `init` be a real, independently-linked binary rather than assembly baked into
//! the kernel's own `.text`.
#![cfg_attr(not(test), no_std)]

use staros_abi::syscall::Syscall;
use staros_ipc::Message;

/// The service protocol tag `init` will answer requests on. Placeholder value.
pub const INIT_PROTOCOL_TAG: u64 = 0x494E_4954; // "INIT"

/// Build the first message `init` intends to send once IPC is live: a `Yield`
/// handshake tagged with our protocol id. Exists so the ABI types have a
/// compiled, testable user today.
#[must_use]
pub fn hello_message() -> (Syscall, Message) {
    (Syscall::Yield, Message::new(INIT_PROTOCOL_TAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_uses_init_tag() {
        let (sc, msg) = hello_message();
        assert_eq!(sc, Syscall::Yield);
        assert_eq!(msg.tag, INIT_PROTOCOL_TAG);
    }
}
