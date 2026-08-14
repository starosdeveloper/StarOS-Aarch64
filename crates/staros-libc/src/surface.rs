//! Shared buffers and IPC, spelled for C.
//!
//! Nothing here is in the Qt contract, and that is the point: these are the calls a
//! *platform plugin* makes, not the calls a portable program makes. A QPA plugin is
//! by definition the layer that knows what system it is on, and the alternative to
//! naming that here is a plugin that issues `svc #0` itself — which puts syscall
//! numbers in two places, and the last time a number in this system lived in two
//! places the two disagreed and a server spent its life rejecting messages it had
//! no business receiving.
//!
//! ## What a surface is, from C
//!
//! ```c
//! unsigned int cap = staros_shared_create(pages);   /* the pages are ours */
//! void *pixels     = staros_shared_map(cap);        /* and mapped here */
//! /* draw */
//! staros_msg_send(display_fd, &commit);             /* the cap travels with it */
//! ```
//!
//! The client allocating its own pixels is the same decision `displaysrv` documents
//! from the other side: the pages belong to whoever made them, the capability is
//! theirs to delegate, and revoking it takes the surface away with no bookkeeping in
//! the server. A server that handed out buffers would have to track who still held
//! them, which is what capabilities exist to avoid.
//!
//! ## Why messages go through a descriptor and not a syscall
//!
//! [`crate::fd`] already turns an endpoint into something `poll` can wait on, and a
//! program that sends through one call and receives through another has two ideas of
//! what a connection is. So the send path is `write` on that descriptor, and this
//! module only adds what a raw `write` cannot carry: the capability handle in the
//! message, which the kernel resolves out of the sender's table and installs in the
//! receiver's.

use core::ffi::{c_int, c_void};

use crate::sys;

/// The message a program exchanges with a service, laid out exactly as the kernel's
/// `Message`: tag, four words, then a capability handle.
///
/// Public and `repr(C)` because a plugin declares this struct in its own header and
/// the two declarations have to agree byte for byte. Four words, not six — getting
/// that wrong puts `cap` sixteen bytes past where the kernel writes it, and the
/// symptom is a server that receives real messages and calls every one malformed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StarosMessage {
    pub tag: u64,
    pub words: [u64; 4],
    pub cap: u32,
}

/// Pages needed for `bytes`, rounded up. Zero bytes is zero pages, which the kernel
/// refuses — a zero-size buffer is always a caller's arithmetic going wrong.
pub(crate) fn pages_for(bytes: usize) -> usize {
    bytes.div_ceil(4096)
}

/// The C entry points.
#[cfg(not(test))]
pub mod exports {
    use super::{c_int, c_void, pages_for, sys, StarosMessage};

    /// `EINVAL`.
    const EINVAL: c_int = 22;
    /// `EBADF`: the descriptor names nothing, or nothing that carries messages.
    const EBADF: c_int = 9;
    /// `EMSGSIZE`: the buffer is not one whole message.
    const EMSGSIZE: c_int = 90;

    /// Allocate `bytes` of memory that can be shared with another process, rounded
    /// up to whole pages, and return its capability handle. Returns 0 on failure —
    /// 0 is never a live handle, so a caller that forgets to check gets a refusal
    /// from the first call that uses it rather than a wrong buffer.
    ///
    /// The pages are **not** mapped by this call. Creating and mapping are separate
    /// because a program may hand a buffer to a server without ever looking at it
    /// itself, and because the address a mapping lands at is the kernel's answer,
    /// not something to be assumed.
    #[no_mangle]
    pub extern "C" fn staros_shared_create(bytes: usize) -> u32 {
        if bytes == 0 {
            return 0;
        }
        sys::create_shared(pages_for(bytes)).unwrap_or(0)
    }

    /// Map a shared buffer into this process and return where it landed, or null.
    ///
    /// Mapping the same buffer twice returns the same address and costs nothing:
    /// the kernel keeps one placement per object. So a plugin that has lost track of
    /// a pointer may simply ask again, which is cheaper than the bookkeeping that
    /// avoids asking.
    #[no_mangle]
    pub extern "C" fn staros_shared_map(cap: u32) -> *mut c_void {
        match sys::map_shared(cap) {
            Some(p) => p.cast::<c_void>(),
            None => core::ptr::null_mut(),
        }
    }

    /// How many bytes a shared buffer holds. Zero for a handle that is not ours.
    ///
    /// A server must ask this rather than believe the message that delegated the
    /// buffer: a size a client can lie about is one that makes the **server** run
    /// off the end of a mapping and take the fault, which is the wrong process
    /// punished for the client's arithmetic.
    #[no_mangle]
    pub extern "C" fn staros_shared_bytes(cap: u32) -> usize {
        sys::shared_pages(cap) * 4096
    }

    /// Send one message on an endpoint descriptor, carrying `msg.cap` if it is not
    /// zero. Returns 0, or a negated errno.
    ///
    /// # Safety
    /// C ABI: `msg` points to one whole [`StarosMessage`].
    #[no_mangle]
    pub unsafe extern "C" fn staros_msg_send(fd: c_int, msg: *const StarosMessage) -> c_int {
        if msg.is_null() {
            return -EINVAL;
        }
        let Some(kind @ crate::fd::Kind::Endpoint { .. }) = crate::fd::get(fd) else {
            return -EBADF;
        };
        // SAFETY: forwarded from the caller; the slice is one message long and the
        // descriptor layer copies out of it before returning.
        let bytes = unsafe {
            core::slice::from_raw_parts(msg.cast::<u8>(), size_of::<StarosMessage>())
        };
        if crate::fd::write(kind, bytes) == size_of::<StarosMessage>() as isize {
            0
        } else {
            -EMSGSIZE
        }
    }

    /// Receive one message, blocking until one arrives. Any capability it carried is
    /// already installed in this process's table and its new handle is in `msg.cap`.
    /// Returns 0, or a negated errno.
    ///
    /// Blocking is the whole behaviour: a caller that does not want to block asks
    /// `poll` first, which is where a timeout belongs and where every other source
    /// it might wait on already is.
    ///
    /// # Safety
    /// C ABI: `msg` points to a [`StarosMessage`] this call fills in.
    #[no_mangle]
    pub unsafe extern "C" fn staros_msg_recv(fd: c_int, msg: *mut StarosMessage) -> c_int {
        if msg.is_null() {
            return -EINVAL;
        }
        let Some(kind @ crate::fd::Kind::Endpoint { .. }) = crate::fd::get(fd) else {
            return -EBADF;
        };
        // SAFETY: forwarded from the caller; the descriptor layer writes exactly one
        // message into it and nothing more.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(msg.cast::<u8>(), size_of::<StarosMessage>())
        };
        if crate::fd::read(kind, bytes) == size_of::<StarosMessage>() as isize {
            0
        } else {
            -EMSGSIZE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_takes_whole_pages_and_never_fewer_than_it_needs() {
        assert_eq!(pages_for(1), 1, "one byte still costs a page");
        assert_eq!(pages_for(4096), 1);
        assert_eq!(pages_for(4097), 2, "one byte over is a second page, not a lost byte");
        // A 64x64 xRGB8888 surface: the size the display demo uses.
        assert_eq!(pages_for(64 * 64 * 4), 4);
        // 640x480, the screen itself — the case a plugin's first backing store hits.
        assert_eq!(pages_for(640 * 480 * 4), 300);
    }

    #[test]
    fn nothing_needs_no_pages() {
        // Zero is not rounded up to one. The kernel refuses a zero-page request, and
        // this must agree with it rather than quietly asking for a page the caller
        // did not want.
        assert_eq!(pages_for(0), 0);
    }

    #[test]
    fn the_message_matches_the_kernels_layout() {
        // A plugin declares this struct in its own header; the kernel writes `cap`
        // at a fixed offset. Six words instead of four moves it sixteen bytes and
        // every message becomes "malformed" with no other symptom.
        assert_eq!(size_of::<StarosMessage>(), 48);
        assert_eq!(core::mem::offset_of!(StarosMessage, tag), 0);
        assert_eq!(core::mem::offset_of!(StarosMessage, words), 8);
        assert_eq!(core::mem::offset_of!(StarosMessage, cap), 40);
    }
}
