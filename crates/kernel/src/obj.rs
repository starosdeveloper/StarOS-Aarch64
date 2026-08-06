//! The kernel object table — the target of every capability, with generational
//! revocation.
//!
//! A [`Cap`](crate::cap::Cap) in a task's table does not name a kernel resource
//! directly; it holds an [`ObjectRef`] into this one global table. The
//! indirection is what makes *cross-task* revocation possible: [`revoke`] bumps
//! the generation of an object's slot, and from that moment **every** capability
//! that referenced it — in any task, including copies handed out over IPC — fails
//! to resolve, because [`get`] checks the generation the reference was minted
//! with against the slot's current one.
//!
//! Without this level of indirection a capability *was* the authority, so a task
//! could only drop its own copy ([the earlier self-only `Revoke`](crate::sched));
//! here the authority lives in the table and a holder of the object can pull it
//! out from under everyone at once.


/// A kernel resource a capability can name.
#[derive(Clone, Copy)]
pub enum Object {
    /// An IPC endpoint, by index into the endpoint table.
    Endpoint {
        /// Index into the kernel endpoint table (see [`crate::ipc`]).
        id: usize,
    },
    /// A device MMIO page, by physical base address.
    Device {
        /// Physical base of the mappable device page.
        phys: u64,
    },
    /// A notification, by index into the kernel notification table (see
    /// [`crate::notify`]). The asynchronous counterpart to an endpoint: it carries
    /// no data, only a signal, and is what the kernel raises to wake a user-space
    /// driver when its interrupt fires.
    Notification {
        /// Index into the kernel notification table.
        id: usize,
    },
    /// A hardware interrupt line the holder may register for and acknowledge, by
    /// its controller interrupt id (`intid`).
    Interrupt {
        /// GIC interrupt id of the line.
        intid: u32,
    },
    /// The authority to mint [`Device`](Object::Device) and
    /// [`Interrupt`](Object::Interrupt) objects for *arbitrary* addresses and
    /// lines. It names no hardware itself — it is the right to name hardware, held
    /// by a privileged user-space device manager. Revoking it (like any object)
    /// stops every holder from minting further, without touching devices already
    /// granted.
    DeviceAuthority,
    /// A shared-memory buffer: one or more physical frames the kernel allocated,
    /// which any holder may map read/write. Unlike a [`Device`](Object::Device),
    /// these frames are RAM the kernel owns and must reclaim — they are freed when
    /// the object table is swept at shutdown (see [`free_reclaimable`]).
    SharedMemory {
        /// Physical base of the shared frames.
        phys: u64,
        /// How many contiguous 4 KiB frames the buffer spans.
        pages: u32,
    },
    /// A DMA buffer: physically contiguous frames the kernel allocated for a
    /// device to read or write directly. Like [`SharedMemory`](Object::SharedMemory)
    /// it is kernel RAM reclaimed at shutdown, but its frames are a single
    /// contiguous run (a device sees physical addresses) and are mapped
    /// non-cacheable.
    DmaBuffer {
        /// Physical base of the contiguous DMA region.
        phys: u64,
        /// How many contiguous 4 KiB frames it spans.
        pages: u32,
    },
}

/// A stable, revocable reference to an [`Object`]: which slot, and the generation
/// that slot held when the reference was created. A reference resolves only while
/// the slot's generation still matches — a [`revoke`] bumps it and invalidates
/// every outstanding reference at once.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    /// Slot index in the object table.
    pub index: u32,
    /// Generation this reference was minted at.
    pub generation: u32,
}

use alloc::vec::Vec;

use staros_mm::{BuddyFrameAllocator, PhysAddr};

use crate::sync::SpinLock;

/// One object-table slot: the live object (if any) and its current generation.
#[derive(Clone, Copy)]
struct Slot {
    generation: u32,
    object: Option<Object>,
}

/// The object table. Every operation below is short, self-contained and touches
/// nothing else, so the lock is simply held for the whole of it.
///
/// It grows on demand rather than being a fixed array: the number of objects a
/// system needs is a property of the hardware it finds, not of a constant chosen
/// here — a phone with thirty devices needs thirty device objects, and a table
/// sized for a demo would simply refuse the thirty-first.
///
/// Growing is safe for this table in a way it is not for every table: a slot's
/// *index* is the only thing an [`ObjectRef`] holds, and a `Vec` never renumbers
/// the elements it already has. Slots are also reused rather than orphaned once
/// [`revoke`] empties them, so a workload that creates and destroys objects in a
/// loop settles at its true high-water mark instead of climbing forever.
static OBJECTS: SpinLock<Vec<Slot>> = SpinLock::new(Vec::new());

/// Create a new object, returning a reference to it, or `None` if the heap is
/// exhausted.
pub fn create(object: Object) -> Option<ObjectRef> {
    let mut slots = OBJECTS.lock();
    // Prefer a revoked slot. Reusing it *keeps its generation*, which is what
    // stops a new object from silently answering to a stale reference minted for
    // the object that used to live there.
    if let Some((i, slot)) = slots.iter_mut().enumerate().find(|(_, s)| s.object.is_none()) {
        slot.object = Some(object);
        return Some(ObjectRef {
            index: i as u32,
            generation: slot.generation,
        });
    }
    // No free slot: grow. `try_reserve` rather than a plain `push` because the
    // caller is reachable from a syscall — user space must not be able to panic
    // the kernel by asking for one object too many.
    slots.try_reserve(1).ok()?;
    slots.push(Slot {
        generation: 0,
        object: Some(object),
    });
    Some(ObjectRef {
        index: (slots.len() - 1) as u32,
        generation: 0,
    })
}

/// Resolve `r` to its live [`Object`], or `None` if the slot is empty, out of
/// range, or has been revoked (its generation has moved past `r`'s).
#[must_use]
pub fn get(r: ObjectRef) -> Option<Object> {
    let slots = OBJECTS.lock();
    let slot = *slots.get(r.index as usize)?;
    if slot.generation == r.generation {
        slot.object
    } else {
        None
    }
}

/// Revoke the object `r` names: clear the slot and bump its generation so every
/// outstanding [`ObjectRef`] to it — in any task — stops resolving. Returns `true`
/// if `r` was live (a matching, present object); `false` if it was already gone
/// or stale (idempotent, and safe against a doubled revoke).
pub fn revoke(r: ObjectRef) -> bool {
    let mut slots = OBJECTS.lock();
    let Some(slot) = slots.get_mut(r.index as usize) else {
        return false;
    };
    if slot.generation == r.generation && slot.object.is_some() {
        // Note: a `SharedMemory` object's frames are not freed here — revoking
        // only stops the capability resolving. The frames are reclaimed by
        // [`free_shared`] at shutdown, when no task can still be mapping them.
        slot.object = None;
        slot.generation = slot.generation.wrapping_add(1);
        true
    } else {
        false
    }
}

/// Free the physical frames backing every [`SharedMemory`](Object::SharedMemory)
/// and [`DmaBuffer`](Object::DmaBuffer) object, returning them to `frames`, and
/// clear those slots.
///
/// Called once at shutdown, after every task has exited: these frames belong to
/// the object, not to any address space (they carry no `SW_OWNED` bit in a task's
/// tables), so task teardown never reclaims them. Doing it here — when no task
/// can still hold a mapping — is what keeps the post-teardown "every frame
/// returned" check honest for shared and DMA buffers.
///
/// Both kinds were allocated as one contiguous run (a shared page is a run of
/// one), so [`BuddyFrameAllocator::free_pages`] on the base reclaims each whole —
/// it recovers the block's size from the tree.
pub fn free_reclaimable(frames: &mut BuddyFrameAllocator) {
    let mut slots = OBJECTS.lock();
    for slot in slots.iter_mut() {
        let base = match slot.object {
            Some(Object::SharedMemory { phys, .. }) | Some(Object::DmaBuffer { phys, .. }) => phys,
            _ => continue,
        };
        frames.free_pages(PhysAddr(base as usize));
        slot.object = None;
        slot.generation = slot.generation.wrapping_add(1);
    }
}
