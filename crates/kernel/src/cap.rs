//! Per-task capabilities — the microkernel's authority model.
//!
//! User space never names kernel objects by raw address or global id; it names
//! them by [`Handle`](staros_abi::Handle), an index into *its own* capability
//! table. A task can only act on objects it was explicitly granted, and can only
//! do what the capability permits (send vs. receive, map a specific device).
//! The kernel resolves the handle against the caller's table on every syscall,
//! so authority is unforgeable: holding index 2 in your table says nothing about
//! index 2 in anyone else's.
//!
//! This replaces the earlier stand-ins — a hard-coded endpoint id and a
//! hard-coded "only the PL011 may be mapped" check — with real, per-task rights
//! granted at spawn time (see `kmain`).
//!
//! A capability is *rights + a reference*, not the object itself: it names its
//! target through an [`ObjectRef`](crate::obj::ObjectRef) into the global
//! [object table](crate::obj), so the object can be revoked out from under every
//! holder at once (generational revocation). The bits stored here say only what
//! the holder is *permitted* to do; whether the object still exists is decided at
//! use time by [`crate::obj::get`].

use alloc::vec::Vec;

use crate::obj::ObjectRef;

/// A single capability: which object it refers to and what the holder may do.
#[derive(Clone, Copy)]
pub enum Cap {
    /// A rendezvous [`crate::ipc`] endpoint, with the directions permitted. A
    /// send-only capability cannot receive and vice versa.
    Endpoint {
        /// Reference to the endpoint object.
        obj: ObjectRef,
        /// The holder may `Send` on this endpoint.
        send: bool,
        /// The holder may `Recv` on this endpoint.
        recv: bool,
    },
    /// The right to map one device MMIO page into the holder's address space via
    /// `MapMemory`.
    Device {
        /// Reference to the device object.
        obj: ObjectRef,
    },
    /// A notification the holder may `Wait` on. Installed into a driver's table by
    /// `IrqRegister`; the kernel signals it when the bound interrupt fires.
    Notification {
        /// Reference to the notification object.
        obj: ObjectRef,
    },
    /// The right to register for and acknowledge one hardware interrupt line
    /// (`IrqRegister`/`IrqAck`).
    Irq {
        /// Reference to the interrupt object.
        obj: ObjectRef,
    },
    /// The right to mint device and interrupt capabilities for arbitrary hardware
    /// (`GrantDevice`/`GrantIrq`). Held by a privileged device manager; this is
    /// what lets device authority live in user space instead of being hard-coded
    /// in the kernel's `main`.
    DeviceAuthority {
        /// Reference to the authority object.
        obj: ObjectRef,
    },
    /// The right to map a shared-memory buffer read/write (`MapShared`). Created
    /// by `CreateShared` and delegable over IPC, so two tasks can share a page.
    Shared {
        /// Reference to the shared-memory object.
        obj: ObjectRef,
    },
    /// The right to map a DMA buffer (`MapDma`) — physically contiguous,
    /// non-cacheable memory a device can read or write directly.
    Dma {
        /// Reference to the DMA-buffer object.
        obj: ObjectRef,
    },
}

impl Cap {
    /// The object this capability refers to, regardless of kind. Used by `Revoke`
    /// to revoke the underlying object for *all* holders.
    #[must_use]
    pub fn object(&self) -> ObjectRef {
        match *self {
            Cap::Endpoint { obj, .. }
            | Cap::Device { obj }
            | Cap::Notification { obj }
            | Cap::Irq { obj }
            | Cap::DeviceAuthority { obj }
            | Cap::Shared { obj }
            | Cap::Dma { obj } => obj,
        }
    }
}

/// A task's capability table, indexed by handle value.
///
/// Grows as the task is granted more. Index 0 is present but permanently `None`:
/// it is the reserved null handle, so that a handle a task never received — a
/// zeroed register, an uninitialised variable — names nothing rather than
/// naming whatever landed in slot 0.
pub type CapTable = Vec<Option<Cap>>;

/// A capability table granting nothing — the default for kernel threads and the
/// base every user grant is built on. `None` if the heap is exhausted.
#[must_use]
pub fn empty_caps() -> Option<CapTable> {
    let mut caps = CapTable::new();
    caps.try_reserve_exact(1).ok()?;
    caps.push(None);
    Some(caps)
}

/// Install `cap` into `table` at the lowest free handle, returning that handle,
/// or `None` if the heap is exhausted.
///
/// Handles are assigned bottom-up from 1 and reused once freed, which is what
/// lets callers stop writing slot numbers: granting in order yields 1, 2, 3, and
/// a capability delegated later lands in the first gap.
pub fn install(table: &mut CapTable, cap: Cap) -> Option<u32> {
    // `skip(1)` keeps the null handle reserved; `position` then counts from the
    // slot after it, so the index is one more than it reports.
    if let Some(free) = table.iter().skip(1).position(Option::is_none) {
        table[free + 1] = Some(cap);
        return Some((free + 1) as u32);
    }
    table.try_reserve(1).ok()?;
    table.push(Some(cap));
    Some((table.len() - 1) as u32)
}
