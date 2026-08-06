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
    /// Map one fresh, zero-filled, read/write page into the caller's address
    /// space and return its virtual address. Anonymous memory a task requests at
    /// runtime — the basis of a growable user heap. No capability is required: a
    /// task may always grow its own memory (until the pool is exhausted).
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
    /// Create a shared-memory buffer: one fresh, zeroed physical page the caller
    /// can share with another task. Returns a *shared capability* handle. No
    /// argument, no authority — any task may create shared memory, exactly as any
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
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrips() {
        for n in 0..=19 {
            let sc = Syscall::from_raw(n).expect("valid number");
            assert_eq!(sc as usize, n);
        }
        assert_eq!(Syscall::from_raw(20), None);
        assert_eq!(Syscall::from_raw(99), None);
    }
}
