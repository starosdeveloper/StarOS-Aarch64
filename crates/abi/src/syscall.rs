//! Syscall numbers.
//!
//! A microkernel exposes only a handful of primitives; policy lives in
//! user space. This enum enumerates the whole surface so the dispatch table in
//! the kernel and the stubs in user space can never drift apart.

/// The syscall number, passed in `x8` on aarch64 by convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Syscall {
    /// Voluntarily yield the CPU to the scheduler.
    Yield = 0,
    /// Send a message to an endpoint and (optionally) block for a reply.
    Send = 1,
    /// Receive a message from an endpoint.
    Recv = 2,
    /// Map a physical memory region into the caller's address space.
    MapMemory = 3,
    /// Terminate the calling task.
    Exit = 4,
    /// Write one byte (in arg0) to the debug console. A minimal, unprivileged
    /// output primitive so early user-space code has a way to be seen.
    DebugPutc = 5,
    /// Revoke the object a capability names (by handle), globally. Every
    /// capability to that object — in any task — stops resolving.
    Revoke = 6,
    /// Register the caller's interrupt capability (arg0 = handle) for delivery:
    /// the kernel creates a notification, binds the interrupt to it, enables the
    /// line at the controller, and returns a fresh notification handle. This is
    /// how a user-space driver arranges to be told its device fired.
    IrqRegister = 7,
    /// Block until a notification (arg0 = handle) is signalled, consuming one
    /// pending signal. Returns immediately if one is already pending.
    Wait = 8,
    /// Acknowledge an interrupt (arg0 = interrupt capability handle): re-enable
    /// the line at the controller now that the driver has serviced the device.
    /// Until this is called the kernel keeps the line masked, so a level-triggered
    /// source cannot storm while the driver runs.
    IrqAck = 9,
    /// Map `arg0` fresh, zero-filled, read/write pages into the caller's address
    /// space and return the virtual address of the first. Anonymous memory a task
    /// requests at runtime — the basis of a growable user heap. No capability is
    /// required: a task may always grow its own memory (until the pool is
    /// exhausted).
    ///
    /// The pages are contiguous **in virtual address space only**; each is a
    /// separate frame. A count of zero is an error rather than a synonym for one,
    /// because a zero here is always a caller's arithmetic having gone wrong, and
    /// the request is capped so a single call cannot drain the pool.
    MapAnon = 10,
    /// Create a new EL0 process from the init image, seeded with the id in arg0.
    /// This is how user space (an `init`) builds the process tree itself, rather
    /// than the kernel hard-coding every task. Returns the child's id.
    Spawn = 11,
    /// Mint a device capability for an arbitrary MMIO page. `arg0` = a
    /// *device-authority* capability handle, `arg1` = the physical base. Only a
    /// task holding the authority may do this — it is how a privileged user-space
    /// device manager hands drivers authority over their `reg` ranges instead of
    /// the kernel hard-coding every device in `main`. Returns the new device
    /// capability's handle.
    GrantDevice = 12,
    /// Mint an interrupt capability for an arbitrary line. `arg0` = a
    /// *device-authority* capability handle, `arg1` = the controller `intid`. The
    /// interrupt counterpart to [`GrantDevice`]. Returns the new interrupt
    /// capability's handle.
    GrantIrq = 13,
    /// Create a shared-memory buffer: `arg0` fresh, zeroed, physically contiguous
    /// pages the caller can share with another task. Returns a *shared capability*
    /// handle. No authority — any task may create shared memory, exactly as any
    /// task may grow its own with [`MapAnon`]. Delegate the returned handle over
    /// IPC and both sides map the same page with [`MapShared`], so real payloads
    /// (a screen frame, a network packet) pass by reference, not by copying six
    /// words per message.
    CreateShared = 14,
    /// Map a shared-memory buffer into the caller's address space, read/write, and
    /// return its virtual address. `arg0` = a *shared* capability handle (created
    /// by [`CreateShared`], possibly received over IPC). Both holders see the same
    /// physical page at their own virtual address.
    MapShared = 15,
    /// Create a DMA buffer: `arg0` = number of 4 KiB pages. Unlike shared memory,
    /// the pages are guaranteed **physically contiguous** (a device sees physical
    /// addresses, and a scattered buffer would need scatter-gather the device may
    /// not have) and are mapped **non-cacheable** so a device writing straight to
    /// RAM and the CPU reading it agree without explicit cache maintenance.
    /// Returns a *DMA* capability handle. No authority — but a real system gates
    /// this behind one; see 2.3.
    CreateDma = 16,
    /// Map a DMA buffer into the caller (a driver), non-cacheable read/write, and
    /// return its virtual address. `arg0` = a *DMA* capability handle. The driver
    /// uses this VA for CPU access; the physical address it programs into the
    /// device is what the buffer was allocated at (and, with an IOMMU, only that
    /// range is what the device is permitted to touch).
    MapDma = 17,
    /// Bind a DMA buffer to a device's IOMMU stream so the SMMU permits that
    /// device to reach **only** those pages and aborts every other address it
    /// emits. `arg0` = a *device-authority* capability handle (programming the
    /// IOMMU is privileged, gated exactly like [`GrantDevice`]), `arg1` = a *DMA*
    /// capability handle (the buffer), `arg2` = the device's StreamID. This is the
    /// enforcement step that makes the DMA-capability model real against a bus
    /// master: without it a driver could point its device at kernel memory. Returns
    /// 0 on success, or an error (e.g. no IOMMU on this machine).
    BindDma = 18,
    /// Write a whole byte buffer to the debug console *atomically*: `arg0` = a
    /// pointer to the bytes in the caller's space, `arg1` = the length. Unlike
    /// [`DebugPutc`], which is one byte per syscall and therefore interleaves
    /// character-by-character when two cores print at once, the entire buffer is
    /// emitted under a single hold of the kernel console lock — so a line printed
    /// with one `DebugWrite` can never be shredded into another core's output.
    /// This is the line-atomic output primitive user space should build lines with;
    /// `DebugPutc` remains for a single stray byte. Returns the number of bytes
    /// written, or an error (unreadable pointer, or a length past the kernel cap).
    DebugWrite = 19,
    /// Read the monotonic clock: nanoseconds since the kernel started counting.
    /// No argument, no capability — reading the time is not a privilege, and a
    /// task that cannot measure elapsed time cannot animate, time out, or profile
    /// itself.
    ///
    /// The value only ever climbs, is common to every task and every core (one
    /// system counter underneath), and is unrelated to wall-clock time: it says
    /// how long since *this boot*, not what the date is. Returns an error only on
    /// a machine whose firmware never declared its counter frequency, which is
    /// the one case where any number would be a guess.
    ClockNow = 20,
    /// Sleep until the monotonic clock reaches `arg0` nanoseconds — the same
    /// scale [`ClockNow`] returns. The task is parked and the CPU given to someone
    /// else; it becomes runnable again once the deadline passes.
    ///
    /// The deadline is **absolute**, not a duration, and that is what makes it
    /// usable for pacing: a loop that sleeps "16 ms" drifts by however long its own
    /// work takes, while one that sleeps until `start + n * 16 ms` does not. A
    /// deadline already in the past returns immediately without parking, so this
    /// doubles as the timeout half of an event loop.
    ///
    /// Returns 0, or an error on a machine with no monotonic clock.
    SleepUntil = 21,
    /// Create a notification of the caller's own and return a capability handle
    /// for it. No authority: signalling is only possible for whoever holds the
    /// capability, so creating one grants nothing on its own.
    ///
    /// The counterpart of [`IrqRegister`], which mints a notification bound to a
    /// hardware line. This one is bound to nothing, and exists so user space can
    /// wake user space — the role `eventfd` plays in a POSIX event loop, and the
    /// only way a task can be roused by something other than a device or a
    /// message.
    NotifyCreate = 22,
    /// Signal the notification named by `arg0`. Wakes its waiter, or is remembered
    /// as one pending signal if nobody is waiting, exactly as an interrupt's
    /// notification behaves. Delegate the capability over IPC and one task can
    /// wake another.
    NotifySignal = 23,
    /// Wait for the first of several notifications, with a deadline: `arg0` = a
    /// pointer to an array of `arg1` capability handles in the caller's memory,
    /// `arg2` = an absolute deadline in the [`ClockNow`] scale (0 means "no
    /// deadline").
    ///
    /// Returns the **index** into that array of the notification that fired, having
    /// consumed one of its pending signals — or [`WouldBlock`] if the deadline
    /// passed first. An index rather than a handle: the caller indexes its own
    /// array with it, and an index cannot be mistaken for an error code.
    ///
    /// This is what an event loop needs and what [`Wait`] cannot do. A loop that
    /// can only wait on one source at a time either misses the others or spins;
    /// `poll` exists on every POSIX system for the same reason.
    ///
    /// [`WouldBlock`]: crate::error::KError::WouldBlock
    WaitAny = 24,
    /// Create a thread in the caller's **own** address space: `arg0` = the EL0
    /// entry point, `arg1` = how many pages of stack to give it, `arg2` = its
    /// thread pointer (`TPIDR_EL0`), `arg3` = a value passed to the entry in the
    /// first argument register. Returns the new task's id.
    ///
    /// The difference from [`Spawn`] is the whole point: `Spawn` builds a fresh
    /// address space from the init image and is therefore a *process*, while this
    /// shares every page and every capability with its creator. A C runtime needs
    /// the second thing and cannot be built out of the first, because threads
    /// sharing a heap is the entire premise.
    ///
    /// Three things do not carry over. The thread gets its own stack (shared pages
    /// would mean two threads writing through one stack), its own thread pointer
    /// (that is what makes `thread_local` work), and a *copy* of the capability
    /// table taken at creation — so a capability minted later is not visible to it.
    SpawnThread = 25,
}

impl Syscall {
    /// Reconstructs a [`Syscall`] from its raw register value, if valid.
    #[must_use]
    pub const fn from_raw(n: usize) -> Option<Syscall> {
        match n {
            0 => Some(Syscall::Yield),
            1 => Some(Syscall::Send),
            2 => Some(Syscall::Recv),
            3 => Some(Syscall::MapMemory),
            4 => Some(Syscall::Exit),
            5 => Some(Syscall::DebugPutc),
            6 => Some(Syscall::Revoke),
            7 => Some(Syscall::IrqRegister),
            8 => Some(Syscall::Wait),
            9 => Some(Syscall::IrqAck),
            10 => Some(Syscall::MapAnon),
            11 => Some(Syscall::Spawn),
            12 => Some(Syscall::GrantDevice),
            13 => Some(Syscall::GrantIrq),
            14 => Some(Syscall::CreateShared),
            15 => Some(Syscall::MapShared),
            16 => Some(Syscall::CreateDma),
            17 => Some(Syscall::MapDma),
            18 => Some(Syscall::BindDma),
            19 => Some(Syscall::DebugWrite),
            20 => Some(Syscall::ClockNow),
            21 => Some(Syscall::SleepUntil),
            22 => Some(Syscall::NotifyCreate),
            23 => Some(Syscall::NotifySignal),
            24 => Some(Syscall::WaitAny),
            25 => Some(Syscall::SpawnThread),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrips() {
        for n in 0..=25 {
            let sc = Syscall::from_raw(n).expect("valid number");
            assert_eq!(sc as usize, n);
        }
        assert_eq!(Syscall::from_raw(26), None);
        assert_eq!(Syscall::from_raw(99), None);
    }
}
