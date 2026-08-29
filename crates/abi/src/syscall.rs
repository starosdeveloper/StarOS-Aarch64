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
    ///
    /// **Where** it lands is the kernel's choice and must be read from the return
    /// value, never assumed. One object keeps one address for the life of a task —
    /// mapping the same buffer twice returns the same pointer and costs nothing —
    /// while a *different* buffer gets a different address, so a server can hold
    /// several at once. Until this rule existed every mapping went to one fixed
    /// address, which is one buffer: a display server's second surface silently
    /// replaced its first, and the symptom was a window drawing another window's
    /// pixels.
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
    /// Create a process from an ELF image **in the caller's own memory**: `arg0` =
    /// a pointer to the bytes, `arg1` = their length, `arg2` = the process id to
    /// seed. Returns the new task's id.
    ///
    /// [`Spawn`] can only start another copy of the one image the kernel was built
    /// with, which makes every program in the system something the kernel shipped.
    /// This takes the image from user space, so a program can be a *file*: read out
    /// of an initramfs, received over IPC, or produced at runtime.
    ///
    /// The kernel deliberately does not learn where the bytes came from. Giving it
    /// a path would mean giving it an archive parser and, later, a filesystem —
    /// the exact thing this system keeps in user space. The caller already knows
    /// how to find its own bytes; the kernel's job is only to turn them into an
    /// address space.
    SpawnImage = 26,
    /// The physical address of a DMA buffer: `arg0` = a *DMA* capability handle.
    ///
    /// [`MapDma`] gives a driver the address *it* uses; a device does not have a
    /// page table and needs the address the memory actually lives at. Until a real
    /// driver existed, the difference never came up — the DMA path was exercised
    /// by a program that only read and wrote the buffer itself. A virtqueue is
    /// memory the *device* walks, and the first thing it must be told is where.
    ///
    /// Disclosing a physical address is not a leak of anything: the caller already
    /// holds the capability, the range is one the kernel allocated for exactly this
    /// purpose, and with an IOMMU it is also the only range that device may reach.
    DmaPhys = 27,
    /// How many 4 KiB pages a shared buffer holds: `arg0` = a *shared* capability
    /// handle.
    ///
    /// [`MapShared`] tells the receiver of a delegated buffer *where* it landed but
    /// not how big it is, so a server had only the sender's word for the size — and
    /// a message is exactly what a server must not trust. Believing an inflated
    /// length makes the **server** run off the end of the mapping and take the
    /// fault, which is the wrong process to punish for a client's lie.
    ///
    /// The page count is not a secret from anyone holding the capability: it is the
    /// size of memory that holder may already read and write in full.
    SharedPages = 28,
    /// The caller's own process id. No arguments; always succeeds.
    ///
    /// A program had no way to name itself. That is fine until a C library has to
    /// answer `getpid`, and it is answered here rather than invented there: a libc
    /// that returned a constant would give two processes the same id, and one that
    /// returned a thread's scheduling id would give two threads of one program
    /// different ones. Both are wrong in ways that only show up in a file name
    /// collision much later.
    ///
    /// Nothing is disclosed by it. The number is the caller's own, it names no
    /// object, and holding it grants nothing — every authority in this system is a
    /// capability, and an id is not one.
    TaskId = 29,
    /// Bind a notification to an endpoint, so that a message arriving there also
    /// signals it: `arg0` = an endpoint capability with **receive** rights, `arg1` =
    /// a notification capability. One notification per endpoint; binding again
    /// replaces the previous one.
    ///
    /// This exists because of a shape mismatch that no amount of user-space code can
    /// paper over. [`Recv`] blocks on *one* endpoint; [`WaitAny`] blocks on a set,
    /// but only of notifications. A program that must wait for a message **and** a
    /// timer **and** a pipe — which is every event loop, and `QEventDispatcherUNIX`
    /// in particular — has no primitive that covers all three.
    ///
    /// The alternative was a thread per endpoint, parked in `Recv`, forwarding into
    /// an eventfd. That works and costs a thread, a stack and a copy of every
    /// message for the sole purpose of changing which primitive the wait is spelled
    /// with. Binding is the same fact expressed once, in the place that already
    /// knows when a message arrives.
    ///
    /// Receive rights are required rather than send rights on purpose: the authority
    /// this hands out is "be told that something is here for you to take", which is
    /// meaningless to a task that may not take it, and is a side channel to a task
    /// that may only send.
    ///
    /// A notification handle of **0 unbinds**, which is the only way to take a
    /// binding back. Without it a program that closes an endpoint descriptor leaves
    /// the kernel signalling a notification whose slot has been handed to something
    /// else, and the new owner gets wake-ups belonging to a connection that no
    /// longer exists.
    EndpointBind = 30,
    /// How many messages are queued at an endpoint right now: `arg0` = an endpoint
    /// capability with **receive** rights. Non-destructive; returns 0 or more.
    ///
    /// A notification cannot answer this, and that is not a detail. [`WaitAny`]
    /// *consumes* the signal it reports, so a poll implementation that treated the
    /// signal as the readiness would have to remember it — and a remembered
    /// readiness bit is precisely how an event loop comes to report a ready
    /// descriptor and then block forever in the read that follows. Readiness has to
    /// be a question asked of the endpoint, every time round the loop.
    EndpointPending = 31,
    /// Signal the notification named by `arg0` when the calling **task** exits, for
    /// any reason: `Exit`, or the kernel killing it after a fault. `arg0` = 0
    /// cancels a previous registration.
    ///
    /// A server cannot clean up after a client that crashed, because nothing tells
    /// it that anything happened. It goes on holding the dead client's surfaces,
    /// which stay on the screen for ever — the most ordinary way a window system
    /// accumulates ghosts. Polling for liveness is the alternative and it is worse:
    /// it costs wake-ups on a system that is doing nothing, and it still cannot tell
    /// "crashed" from "busy".
    ///
    /// The authority flows the way capabilities require: the *client* creates the
    /// notification and delegates it to the server over IPC. Nobody can ask to be
    /// told about a task that did not offer, so this discloses nothing — and a
    /// client that simply never registers is a client the server cannot clean up
    /// after, which is honest. A server that must not depend on client goodwill has
    /// to bound what a client can hold, and that is a different mechanism.
    ///
    /// Registered per **task**, not per process: it fires when the task that
    /// registered it exits. A thread of a multi-threaded program dying is not that
    /// program dying, so a library registers on the thread whose death means the
    /// program is over.
    NotifyOnExit = 32,
    /// Send exactly as [`Send`] does, but return [`WouldBlock`] instead of parking
    /// when the endpoint has no room: `arg0` = endpoint handle, `arg1` = message.
    ///
    /// [`Send`] blocking is right for a driver, and that is why it does: a driver
    /// that dropped events silently gives a keyboard which occasionally misses a
    /// keystroke, and back-pressure that stops the driver is a failure with a shape.
    ///
    /// It is wrong for a **server pushing to a client**, and the difference is who
    /// is punished. A display server delivering a pointer event to whatever window
    /// is under the pointer is delivering to a program it knows nothing about —
    /// which may never read its event endpoint at all, and several in this tree do
    /// not. One of those, moved over by an idle pointer, parks the compositor for
    /// ever: the screen stops, every other window stops with it, and the input
    /// driver blocks behind it in turn. That happened, and from the outside it looked
    /// like a keyboard that worked once and then stopped.
    ///
    /// So a server pushing unsolicited events uses this and counts what it drops. A
    /// client that is not reading its events is a client with no use for them.
    ///
    /// [`Send`]: Syscall::Send
    /// [`WouldBlock`]: crate::error::KError::WouldBlock
    SendNoWait = 33,

    /// Unmap `x1` pages at `x0` from the caller's address space, returning how many
    /// were mapped and are now not. Frames the space privately owns go back to the
    /// pool; shared, DMA and device pages are only unmapped, their frames belonging
    /// to an object or to hardware.
    ///
    /// Until this existed, **nothing in this system ever gave memory back short of
    /// dying.** `munmap` in the C library was a counter — `staros_mmap_retained()`
    /// reported what had leaked, which made the hole a number instead of a rumour,
    /// and that is all it could do. The cost is ordinary rather than exotic: a
    /// window that resizes allocates two buffers and abandons two, so dragging one
    /// edge of it leaks megabytes a second.
    ///
    /// Addresses that map nothing are skipped, not refused: unmapping a partly-free
    /// range is what an allocator does while it coalesces, and the count says what
    /// actually happened.
    ///
    /// What this does **not** return is address space. The heap cursor moves forward
    /// only, so freed addresses are not handed out again — 47 bits of window against
    /// frames that are megabytes each, and the day a program walks to the end of the
    /// region is the day that changes.
    Unmap = 34,

    /// Receive from the endpoint `x0` names into the message at `x1`, giving up
    /// when the monotonic clock reaches the absolute deadline in `x2`. Zero means
    /// no deadline, which is exactly [`Recv`](Syscall::Recv).
    ///
    /// Returns 0 on delivery, or [`WouldBlock`] when the deadline passed with
    /// nothing to take — the same answer [`WaitAny`](Syscall::WaitAny) gives for
    /// the same situation, because it is the same situation.
    ///
    /// Absolute, for the reason `SleepUntil` is: a duration is measured from
    /// whenever the call gets to run, so a caller preempted between working it out
    /// and asking waits longer than it meant to and cannot tell.
    ///
    /// Until this existed, waiting with a bound meant binding a notification to the
    /// endpoint and using `WaitAny` — three syscalls and a notification per
    /// endpoint to say "wait, but not forever". A client waiting for an answer that
    /// may never come is an ordinary thing to be, not an advanced one.
    ///
    /// [`WouldBlock`]: crate::error::KError::WouldBlock
    RecvUntil = 35,
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
            26 => Some(Syscall::SpawnImage),
            27 => Some(Syscall::DmaPhys),
            28 => Some(Syscall::SharedPages),
            29 => Some(Syscall::TaskId),
            30 => Some(Syscall::EndpointBind),
            31 => Some(Syscall::EndpointPending),
            32 => Some(Syscall::NotifyOnExit),
            33 => Some(Syscall::SendNoWait),
            34 => Some(Syscall::Unmap),
            35 => Some(Syscall::RecvUntil),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrips() {
        for n in 0..=35 {
            let sc = Syscall::from_raw(n).expect("valid number");
            assert_eq!(sc as usize, n);
        }
        assert_eq!(Syscall::from_raw(36), None);
        assert_eq!(Syscall::from_raw(99), None);
    }
}
