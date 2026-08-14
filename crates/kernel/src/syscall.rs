//! Syscall dispatch — the kernel side of the SVC trap.
//!
//! The arch layer decodes the calling convention and hands us a
//! [`SyscallRequest`]; here we map the number to an [`abi::Syscall`] and
//! implement its behaviour. This is *policy*, deliberately kept out of the
//! architecture crate. The function is bound to the arch trap by name at link
//! time (see `staros_arch_aarch64::exceptions`).

use core::mem::size_of;

use staros_abi::error::KError;
use staros_abi::syscall::Syscall;
use staros_abi::Handle;
use staros_arch_aarch64::exceptions::SyscallRequest;
use staros_arch_aarch64::timer;
use staros_ipc::Message;
use staros_mm::PAGE_SIZE;

use crate::cap::Cap;
use crate::ipc::KMessage;
use crate::obj::{self, Object};
use crate::{ipc, irq, notify, sched};
use crate::console::klog;

/// Kernel syscall dispatcher, invoked from the arch SVC trap.
///
/// Returns the value the trap path writes into `x0`: non-negative on success,
/// or a negative [`KError`] code on failure.
#[no_mangle]
pub extern "Rust" fn staros_syscall_dispatch(req: &SyscallRequest) -> isize {
    match Syscall::from_raw(req.number) {
        // Reschedule the caller. Returns to it (0 = success) once it is picked
        // again; a no-op if nothing else is runnable.
        Some(Syscall::Yield) => {
            sched::yield_now();
            0
        }

        // Terminate the calling task. Each EL0 process is backed by a scheduler
        // task, so `Exit` ends that task and switches to the next runnable one
        // (or back to the bootstrap thread when the last one finishes). Never
        // returns to the caller.
        Some(Syscall::Exit) => sched::exit(),

        // Minimal console output primitive: write the low byte of arg0. This is
        // what early user-space uses to be seen; usable from EL0.
        Some(Syscall::DebugPutc) => {
            // One byte, but still through the console lock: a user program
            // printing a string does so one syscall at a time, and its bytes must
            // not be shuffled into another core's line.
            crate::console::putc(req.args[0] as u8);
            0
        }

        // Line-atomic console output: emit the whole `(ptr, len)` buffer under a
        // single hold of the console lock, so a user program's line cannot be
        // shredded into another core's output the way byte-at-a-time `DebugPutc`
        // is. The pointer is walked in the caller's own tables before we touch it
        // (as every user pointer is), and the length is capped so one task cannot
        // monopolise the lock with an enormous write.
        Some(Syscall::DebugWrite) => {
            /// Upper bound on a single line-atomic write. A boot log line is tens of
            /// bytes; this leaves generous headroom without letting a caller pin the
            /// console lock for an unbounded span.
            const MAX_DEBUG_WRITE: usize = 4096;
            let ptr = req.args[0];
            let len = req.args[1] as usize;
            if len == 0 {
                return 0;
            }
            if len > MAX_DEBUG_WRITE {
                return KError::InvalidArgument.as_raw();
            }
            // No alignment requirement here (unlike `user_ptr_ok`): a string is not
            // 8-byte aligned. We only need every byte to be mapped and EL0-readable
            // in the caller's active space, which this walk of its tables confirms.
            if !sched::current_range_ok(ptr, len, false) {
                return KError::InvalidArgument.as_raw();
            }
            // Copy the string out of EL0 memory into a kernel buffer via unprivileged
            // loads, then emit it. A direct EL1 dereference would fault under PAN on
            // real hardware (Cortex-A76); `copy_from_user` uses `LDTR`, which does not.
            let mut buf = [0u8; MAX_DEBUG_WRITE];
            // SAFETY: the caller's address space is active and the walk above
            // confirmed all `len` bytes are mapped EL0-readable; `len <= buf.len()`.
            unsafe { staros_arch_aarch64::usercopy::copy_from_user(&mut buf[..len], ptr) };
            crate::console::write_bytes(&buf[..len]);
            len as isize
        }

        // Send a message: `x0` = endpoint *handle*, `x1` = pointer to a user
        // `Message`. If the message names a capability (`Message.cap`), that
        // capability is resolved from the caller's table and transferred.
        Some(Syscall::Send) => {
            let id = match sched::resolve_cap(req.args[0] as u32) {
                Some(Cap::Endpoint { obj, send: true, .. }) => match obj::get(obj) {
                    Some(Object::Endpoint { id }) => id,
                    // Object revoked (or somehow not an endpoint): the handle no
                    // longer names a live object.
                    _ => return KError::BadHandle.as_raw(),
                },
                Some(_) => return KError::PermissionDenied.as_raw(),
                None => return KError::BadHandle.as_raw(),
            };
            let Some(msg) = read_user_msg(req.args[1]) else {
                return KError::InvalidArgument.as_raw();
            };
            // Resolve any transferred capability from the sender's own space.
            let cap = if msg.cap.is_null() {
                None
            } else {
                match sched::resolve_cap(msg.cap.0) {
                    Some(c) => Some(c),
                    None => return KError::BadHandle.as_raw(),
                }
            };
            ipc::send(id, KMessage { msg, cap })
        }

        // Receive a message: `x0` = endpoint *handle*, `x1` = pointer to a user
        // `Message` to fill. A transferred capability is installed into the
        // receiver's table and its new handle written into `Message.cap`.
        Some(Syscall::Recv) => {
            let id = match sched::resolve_cap(req.args[0] as u32) {
                Some(Cap::Endpoint { obj, recv: true, .. }) => match obj::get(obj) {
                    Some(Object::Endpoint { id }) => id,
                    _ => return KError::BadHandle.as_raw(),
                },
                Some(_) => return KError::PermissionDenied.as_raw(),
                None => return KError::BadHandle.as_raw(),
            };
            let km = match ipc::recv(id) {
                Ok(km) => km,
                Err(e) => return e.as_raw(),
            };
            let mut msg = km.msg;
            msg.cap = match km.cap {
                Some(cap) => match sched::install_cap_current(cap) {
                    Some(h) => Handle(h),
                    None => return KError::OutOfResources.as_raw(),
                },
                None => Handle::NULL,
            };
            if write_user_msg(req.args[1], msg) {
                0
            } else {
                KError::InvalidArgument.as_raw()
            }
        }

        // Map an MMIO region into the caller's address space and return its user
        // VA: `x0` = a *device capability* handle. Only a task granted the device
        // capability can map it — this is what safely lets a driver live in user
        // space. Denials are logged so enforcement is visible.
        Some(Syscall::MapMemory) => {
            // Resolve to a live device object: the handle must name a device
            // capability *and* the underlying object must not have been revoked.
            let outcome = match sched::resolve_cap(req.args[0] as u32) {
                Some(Cap::Device { obj }) => match obj::get(obj) {
                    Some(Object::Device { phys }) => Ok(phys),
                    // The device object was revoked (possibly by another task).
                    _ => Err(("object was revoked", KError::BadHandle)),
                },
                Some(_) => Err(("handle is not a device capability", KError::PermissionDenied)),
                None => Err(("no such capability", KError::BadHandle)),
            };
            match outcome {
                Ok(phys) => sched::map_device_current(phys),
                Err((reason, err)) => {
                    klog!(
                        "[cap] task {} denied MapMemory(handle {}): {reason}",
                        sched::current_id(),
                        req.args[0],
                    );
                    err.as_raw()
                }
            }
        }

        // Revoke the *object* a handle names: `x0` = handle. Unlike a self-only
        // slot drop, this bumps the object's generation in the global table, so
        // every capability to it — in any task, including copies delegated over
        // IPC — stops resolving. This is how a holder pulls authority back from
        // everyone at once.
        Some(Syscall::Revoke) => match sched::resolve_cap(req.args[0] as u32) {
            Some(cap) => {
                if obj::revoke(cap.object()) {
                    0
                } else {
                    KError::BadHandle.as_raw()
                }
            }
            None => KError::BadHandle.as_raw(),
        },

        // Register the caller's interrupt capability (`x0` = handle) for delivery.
        // The kernel mints a notification, binds the interrupt to it, enables the
        // line, and installs a notification capability into the caller's table,
        // returning its new handle. Lets a user-space driver be woken by hardware.
        Some(Syscall::IrqRegister) => {
            let intid = match resolve_interrupt(req.args[0] as u32) {
                Ok(intid) => intid,
                Err(e) => return e.as_raw(),
            };
            let Some(notif_id) = notify::create() else {
                return KError::OutOfResources.as_raw();
            };
            let Some(obj_ref) = obj::create(Object::Notification { id: notif_id }) else {
                return KError::OutOfResources.as_raw();
            };
            let Some(handle) = sched::install_cap_current(Cap::Notification { obj: obj_ref }) else {
                return KError::OutOfResources.as_raw();
            };
            if !irq::register(intid, notif_id) {
                return KError::InvalidArgument.as_raw();
            }
            handle as isize
        }

        // Block until the notification named by `x0` is signalled (consuming one
        // pending signal). This is how a driver sleeps until its device fires.
        Some(Syscall::Wait) => match resolve_notification(req.args[0] as u32) {
            Ok(id) => {
                notify::wait(id);
                0
            }
            Err(e) => e.as_raw(),
        },

        // Create a notification of the caller's own and return its capability
        // handle. Bound to no hardware — this is how user space wakes user space.
        Some(Syscall::NotifyCreate) => {
            let Some(notif_id) = notify::create() else {
                return KError::OutOfResources.as_raw();
            };
            let Some(obj_ref) = obj::create(Object::Notification { id: notif_id }) else {
                return KError::OutOfResources.as_raw();
            };
            match sched::install_cap_current(Cap::Notification { obj: obj_ref }) {
                Some(handle) => handle as isize,
                None => {
                    obj::revoke(obj_ref);
                    KError::OutOfResources.as_raw()
                }
            }
        }

        // Signal the notification named by `x0`. Holding the capability is the
        // whole authority: a task can only wake what it was given.
        Some(Syscall::NotifySignal) => match resolve_notification(req.args[0] as u32) {
            Ok(id) => {
                notify::signal(id);
                0
            }
            Err(e) => e.as_raw(),
        },

        // The physical address of a DMA buffer (`x0` = a DMA capability handle) —
        // the one thing a driver must tell its device and the one address
        // `MapDma` cannot give it, since a device has no page table.
        Some(Syscall::DmaPhys) => match sched::resolve_cap(req.args[0] as u32) {
            Some(Cap::Dma { obj }) => match obj::get(obj) {
                Some(Object::DmaBuffer { phys, .. }) => phys as isize,
                _ => KError::BadHandle.as_raw(),
            },
            Some(_) => KError::PermissionDenied.as_raw(),
            None => KError::BadHandle.as_raw(),
        },

        // Create a process from an ELF image the caller holds: `x0` = pointer,
        // `x1` = length, `x2` = the id to seed. The kernel never learns where the
        // bytes came from — giving it a path would mean giving it an archive
        // parser, and that lives in user space here.
        Some(Syscall::SpawnImage) => {
            /// Ceiling on an image handed over in one call. The bytes are copied
            /// into the kernel heap to be parsed, so this is a real limit on kernel
            /// memory, not a policy: a bigger program needs the loader to stream
            /// segments straight from the caller's pages instead, which is work for
            /// the phase that has a program that big.
            const MAX_IMAGE_BYTES: usize = 256 * 1024;
            let ptr = req.args[0];
            let len = req.args[1] as usize;
            if len == 0 || len > MAX_IMAGE_BYTES {
                return KError::InvalidArgument.as_raw();
            }
            if !sched::current_range_ok(ptr, len, false) {
                return KError::InvalidArgument.as_raw();
            }
            crate::spawn_image(ptr, len, req.args[2] as u8)
        }

        // Create a thread in the caller's own address space: `x0` = EL0 entry,
        // `x1` = stack pages, `x2` = thread pointer, `x3` = argument. No
        // capability: a task may always divide its own time and its own memory,
        // exactly as `MapAnon` lets it grow that memory.
        Some(Syscall::SpawnThread) => {
            /// A thread's stack is fixed-size (no demand growth outside the address
            /// space's own stack region), so the ceiling is what a thread may claim
            /// up front rather than what it may ever use. 64 pages is 256 KiB — the
            /// same limit the main stack grows to.
            const MAX_THREAD_STACK_PAGES: u64 = 64;
            let pages = req.args[1];
            if pages == 0 || pages > MAX_THREAD_STACK_PAGES {
                return KError::InvalidArgument.as_raw();
            }
            sched::spawn_thread(req.args[0], pages, req.args[2], req.args[3])
        }

        // Wait for the first of `x1` notifications (handles at `x0`) to fire, or
        // until the absolute deadline in `x2`. Returns the index of whichever
        // fired, or `WouldBlock` on timeout.
        Some(Syscall::WaitAny) => wait_any(req.args[0], req.args[1] as usize, req.args[2]),

        // Acknowledge the interrupt named by `x0`: re-enable the line now the
        // driver has serviced the device. Undoes the mask the forwarding path set.
        Some(Syscall::IrqAck) => {
            let intid = match resolve_interrupt(req.args[0] as u32) {
                Ok(intid) => intid,
                Err(e) => return e.as_raw(),
            };
            irq::ack(intid);
            0
        }

        // Read the monotonic clock: nanoseconds since the kernel started counting.
        // No capability, exactly like `MapAnon` needs none — time is not a
        // privilege, and a task with no way to measure elapsed time can neither
        // animate nor time out nor profile itself.
        //
        // The width is deliberate. Nanoseconds in an `isize` stay positive (and
        // therefore stay distinguishable from an error code) for 292 years of
        // uptime, and the value is measured from the kernel's own start rather
        // than from whatever the firmware had already counted — so it begins near
        // zero on every boot and the headroom is real rather than nominal.
        Some(Syscall::ClockNow) => match timer::monotonic_ns() {
            Some(ns) => ns as isize,
            // Only reachable on a machine that never declared `CNTFRQ_EL0`. The
            // honest answer is "this machine has no clock", not a plausible zero:
            // a caller that gets zero twice concludes no time passed, while one
            // that gets an error knows not to ask again.
            None => KError::NotSupported.as_raw(),
        },

        // Park the caller until the monotonic clock reaches `x0`. No capability:
        // giving up the CPU until a time is no more a privilege than reading the
        // clock. The policy — absolute deadline, no parking if already late — lives
        // in `sched::sleep_until`.
        Some(Syscall::SleepUntil) => sched::sleep_until(req.args[0]),

        // Grow the caller's memory: map `x0` fresh zero pages, contiguous in
        // virtual address space, and return the VA of the first. No capability
        // needed — a task may always allocate its own memory.
        //
        // Zero is rejected rather than quietly meaning one. A count of zero is
        // always a caller's arithmetic going wrong (a length that underflowed, a
        // loop that should not have run), and answering it with a page hides that
        // at the exact moment it could still be caught.
        Some(Syscall::MapAnon) => {
            // A ceiling, so one call cannot take the whole pool in a single step
            // and starve every other task before the allocator can say no. Large
            // enough that a C heap grows in useful bites: 4 MiB per call turns the
            // 4096 syscalls that 16 MiB used to cost into eight.
            const MAX_ANON_PAGES: u64 = 1024;
            let pages = req.args[0];
            if pages == 0 || pages > MAX_ANON_PAGES {
                return KError::InvalidArgument.as_raw();
            }
            sched::map_anon_current(pages)
        }

        // Create a child EL0 process from the init image, seeded with the id in
        // `x0`. User space builds its own process tree instead of the kernel
        // hard-coding every task. Returns the child's id, or an error if the
        // image, frame pool, or scheduler cannot accommodate it.
        Some(Syscall::Spawn) => {
            if crate::spawn_child(req.args[0] as u8) {
                req.args[0] as isize
            } else {
                KError::OutOfResources.as_raw()
            }
        }

        // Mint a device capability for an arbitrary MMIO page: `x0` = a
        // device-authority handle, `x1` = the physical base. The holder of the
        // authority — a privileged user-space device manager — is trusted to name
        // hardware; the kernel only checks it *holds* the authority, then mints
        // the object and a capability for it into the caller's table. This is what
        // moves device authority out of the kernel's `main` and into user space.
        Some(Syscall::GrantDevice) => {
            if let Err(e) = check_authority(req.args[0] as u32) {
                return e.as_raw();
            }
            grant(Object::Device { phys: req.args[1] }, |obj| Cap::Device { obj })
        }

        // Mint an interrupt capability for an arbitrary line: `x0` = a
        // device-authority handle, `x1` = the controller `intid`. The interrupt
        // counterpart to `GrantDevice`.
        Some(Syscall::GrantIrq) => {
            if let Err(e) = check_authority(req.args[0] as u32) {
                return e.as_raw();
            }
            grant(Object::Interrupt { intid: req.args[1] as u32 }, |obj| Cap::Irq { obj })
        }

        // Create a shared-memory buffer: allocate `x0` zeroed frames, wrap them in
        // a shared object, and hand the caller a shared capability. No authority —
        // any task may make memory to share, as any task may grow its own.
        Some(Syscall::CreateShared) => create_shared(req.args[0] as usize),

        // Map a shared buffer (`x0` = shared capability handle) into the caller,
        // read/write, and return its VA. Both holders of the capability map the
        // same physical page.
        Some(Syscall::MapShared) => match sched::resolve_cap(req.args[0] as u32) {
            Some(Cap::Shared { obj }) => match obj::get(obj) {
                Some(Object::SharedMemory { phys, pages }) => {
                    sched::map_shared_current(obj, phys, pages)
                }
                _ => KError::BadHandle.as_raw(),
            },
            Some(_) => KError::PermissionDenied.as_raw(),
            None => KError::BadHandle.as_raw(),
        },

        // How big a shared buffer is, in pages (`x0` = shared capability handle).
        // A server that receives a delegated buffer needs this to bound what it
        // copies: the only other source for the size is the message that came with
        // it, and a size a client can lie about is one that makes the *server*
        // fault. Holding the capability is the whole permission — the answer
        // describes memory the caller may already read and write in full.
        Some(Syscall::SharedPages) => match sched::resolve_cap(req.args[0] as u32) {
            Some(Cap::Shared { obj }) => match obj::get(obj) {
                Some(Object::SharedMemory { pages, .. }) => pages as isize,
                _ => KError::BadHandle.as_raw(),
            },
            Some(_) => KError::PermissionDenied.as_raw(),
            None => KError::BadHandle.as_raw(),
        },

        // The caller's own process id — its task id, or its creator's if it is a
        // thread. Needs no capability: the number names no object and grants
        // nothing, and it is the caller's own.
        Some(Syscall::TaskId) => sched::current_pid() as isize,

        // Bind the notification named by `x1` to the endpoint named by `x0`, so a
        // message arriving there also signals it. Both handles are the caller's, and
        // the endpoint one must carry receive rights: this grants the authority to
        // be *told* about messages, which belongs to whoever may take them.
        Some(Syscall::EndpointBind) => {
            let ep = match resolve_receivable_endpoint(req.args[0] as u32) {
                Ok(id) => id,
                Err(e) => return e.as_raw(),
            };
            let notif = match resolve_notification(req.args[1] as u32) {
                Ok(id) => id,
                Err(e) => return e.as_raw(),
            };
            if ipc::bind_notify(ep, notif) {
                0
            } else {
                KError::BadHandle.as_raw()
            }
        }

        // How many messages are queued at the endpoint named by `x0`. The question
        // `poll` asks each time round its loop, and the reason it need not remember
        // a readiness it was told about once.
        Some(Syscall::EndpointPending) => match resolve_receivable_endpoint(req.args[0] as u32) {
            Ok(id) => ipc::pending(id) as isize,
            Err(e) => e.as_raw(),
        },

        // Allocate a physically-contiguous, non-cacheable DMA buffer of `x0`
        // pages and hand the caller a DMA capability for it.
        Some(Syscall::CreateDma) => create_dma(req.args[0] as usize),

        // Map a DMA buffer (`x0` = DMA capability handle) into the caller,
        // non-cacheable read/write, and return its VA.
        Some(Syscall::MapDma) => match sched::resolve_cap(req.args[0] as u32) {
            Some(Cap::Dma { obj }) => match obj::get(obj) {
                Some(Object::DmaBuffer { phys, pages }) => sched::map_dma_current(phys, pages),
                _ => KError::BadHandle.as_raw(),
            },
            Some(_) => KError::PermissionDenied.as_raw(),
            None => KError::BadHandle.as_raw(),
        },

        // Bind a DMA buffer to a device StreamID at the IOMMU: `x0` = a
        // device-authority handle (programming the SMMU is privileged, so it is
        // gated exactly like GrantDevice), `x1` = the DMA capability naming the
        // buffer, `x2` = the device's StreamID. This is the enforcement step — the
        // SMMU will then let that device reach only these pages.
        Some(Syscall::BindDma) => bind_dma(req.args[0] as u32, req.args[1] as u32, req.args[2] as u32),

        // Number outside the ABI entirely.
        None => KError::NoSuchSyscall.as_raw(),
    }
}

/// Allocate `pages` zeroed frames, make a shared-memory object from them, and
/// install a shared capability for it into the caller's table. Returns the handle,
/// or an error if the request is out of range or memory is exhausted (unwinding
/// whatever it already took so nothing leaks).
///
/// The run is physically contiguous, like a DMA buffer and unlike anonymous
/// memory — but for a different reason. Nothing here has to be contiguous for the
/// *hardware*; it has to be contiguous because a shared object is described by one
/// `(phys, pages)` pair and mapped into each holder from that description. A
/// scattered buffer would need the object to carry a frame list, which is a
/// bigger change than this pays for. The visible consequence is the ceiling below:
/// contiguous allocation rounds up to a power of two, so a large request can fail
/// on merely fragmented memory.
fn create_shared(pages: usize) -> isize {
    /// One 1080p frame of xRGB8888, which is what a full-screen backing store is.
    ///
    /// It was 64 pages, and the comment claimed that was "enough for a screenful at
    /// modest size" — 256 KiB, against the 1.2 MB this machine's own 640x480 screen
    /// needs. Nothing noticed while the only client drew 64x64 squares. The first
    /// window sized to the screen would have met `InvalidArgument` and had no idea
    /// why, because the number was wrong rather than the request.
    ///
    /// The real constraint is not the size, it is that these frames are
    /// **physically contiguous** — `Object::SharedMemory` holds one address — so a
    /// pool with 60 MiB free in scattered pieces can still refuse 8 MiB. Nothing
    /// here needs contiguity; only DMA does. Until that changes, a full-screen
    /// buffer is a request that can fail for reasons the caller cannot see, and the
    /// honest answer is `OutOfResources` rather than a smaller buffer it did not
    /// ask for.
    const MAX_SHARED_PAGES: usize = 2048;
    // Zero is a caller's arithmetic going wrong, not a request for nothing.
    if pages == 0 || pages > MAX_SHARED_PAGES {
        return KError::InvalidArgument.as_raw();
    }
    let Some(phys) = crate::mem::with(|f| f.alloc_pages(pages)) else {
        return KError::OutOfResources.as_raw();
    };
    // Memory handed to two tasks must start clean, not carrying whatever a
    // previous owner left in the frames. Every page, not just the first: the
    // second page of a buffer is exactly where a stale secret would survive
    // unnoticed, because nothing routinely reads it.
    // SAFETY: the run is in the kernel's linear map and uniquely ours until we
    // publish it; zeroing `pages` pages at its linear-map address is sound.
    unsafe {
        let va = staros_arch_aarch64::mmu::phys_to_virt(phys.0 as u64) as *mut u8;
        core::ptr::write_bytes(va, 0, pages * PAGE_SIZE);
    }
    let obj = Object::SharedMemory { phys: phys.0 as u64, pages: pages as u32 };
    let Some(obj_ref) = obj::create(obj) else {
        crate::mem::with(|f| f.free_pages(phys));
        return KError::OutOfResources.as_raw();
    };
    match sched::install_cap_current(Cap::Shared { obj: obj_ref }) {
        Some(handle) => handle as isize,
        None => {
            obj::revoke(obj_ref);
            crate::mem::with(|f| f.free_pages(phys));
            KError::OutOfResources.as_raw()
        }
    }
}

/// Allocate `pages` physically-contiguous, zeroed frames, make a DMA object from
/// them, and install a DMA capability into the caller's table. Returns the handle,
/// or an error if the request is out of range or memory is exhausted (unwinding
/// what it took).
fn create_dma(pages: usize) -> isize {
    // A driver's ring or frame buffer, not an arbitrary memory grab. Contiguous
    // allocation rounds up to a power of two, so keep the ceiling modest.
    const MAX_DMA_PAGES: usize = 256; // 1 MiB
    if pages == 0 || pages > MAX_DMA_PAGES {
        return KError::InvalidArgument.as_raw();
    }
    let Some(phys) = crate::mem::with(|f| f.alloc_pages(pages)) else {
        return KError::OutOfResources.as_raw();
    };
    // SAFETY: the run is in the kernel's linear map and uniquely ours until
    // published; zeroing `pages` pages at its linear-map address is sound.
    unsafe {
        let va = staros_arch_aarch64::mmu::phys_to_virt(phys.0 as u64) as *mut u8;
        core::ptr::write_bytes(va, 0, pages * PAGE_SIZE);
    }
    let obj = Object::DmaBuffer { phys: phys.0 as u64, pages: pages as u32 };
    let Some(obj_ref) = obj::create(obj) else {
        crate::mem::with(|f| f.free_pages(phys));
        return KError::OutOfResources.as_raw();
    };
    match sched::install_cap_current(Cap::Dma { obj: obj_ref }) {
        Some(handle) => handle as isize,
        None => {
            obj::revoke(obj_ref);
            crate::mem::with(|f| f.free_pages(phys));
            KError::OutOfResources.as_raw()
        }
    }
}

/// Bind a DMA buffer to a device StreamID at the IOMMU. `auth` must be a live
/// device-authority handle (programming the SMMU is privileged); `dma` names the
/// buffer; `streamid` is the device. Returns `0` on success, or an error —
/// notably [`KError::NotSupported`] when the machine has no IOMMU, so a demo can
/// run on plain `virt` and learn there is nothing to bind against.
fn bind_dma(auth: u32, dma: u32, streamid: u32) -> isize {
    if let Err(e) = check_authority(auth) {
        return e.as_raw();
    }
    let (phys, pages) = match sched::resolve_cap(dma) {
        Some(Cap::Dma { obj }) => match obj::get(obj) {
            Some(Object::DmaBuffer { phys, pages }) => (phys, pages as usize),
            _ => return KError::BadHandle.as_raw(),
        },
        Some(_) => return KError::PermissionDenied.as_raw(),
        None => return KError::BadHandle.as_raw(),
    };
    match crate::iommu::bind(streamid, phys, pages) {
        Ok(()) => 0,
        Err(e) => e.as_raw(),
    }
}

/// Confirm the caller holds a *live* device-authority capability at `handle`.
///
/// The whole grant model rests on this one check: minting is gated by holding the
/// authority, nothing else — no process id, no allow-list. Revoking the authority
/// object (its generation bumps) makes `obj::get` fail here, so a device manager
/// can be de-authorised at runtime like any other holder.
fn check_authority(handle: u32) -> Result<(), KError> {
    match sched::resolve_cap(handle) {
        Some(Cap::DeviceAuthority { obj }) => match obj::get(obj) {
            Some(Object::DeviceAuthority) => Ok(()),
            _ => Err(KError::BadHandle),
        },
        Some(_) => Err(KError::PermissionDenied),
        None => Err(KError::BadHandle),
    }
}

/// Create `object`, wrap it in a capability with `to_cap`, and install that into
/// the caller's table. Returns the new handle, or `OutOfResources` if any step
/// exhausts the heap. Shared by `GrantDevice` and `GrantIrq`, whose only
/// difference is which object and capability kind they mint.
fn grant(object: Object, to_cap: impl FnOnce(obj::ObjectRef) -> Cap) -> isize {
    let Some(obj_ref) = obj::create(object) else {
        return KError::OutOfResources.as_raw();
    };
    match sched::install_cap_current(to_cap(obj_ref)) {
        Some(handle) => handle as isize,
        None => {
            // Could not hand the caller a handle: don't leave an orphaned object
            // behind that nothing can name or revoke.
            obj::revoke(obj_ref);
            KError::OutOfResources.as_raw()
        }
    }
}

/// Resolve an interrupt-capability handle to its live line's `intid`, or a
/// [`KError`] explaining why it does not (wrong kind, revoked, or absent). Shared
/// by `IrqRegister` and `IrqAck`.
fn resolve_interrupt(handle: u32) -> Result<u32, KError> {
    match sched::resolve_cap(handle) {
        Some(Cap::Irq { obj }) => match obj::get(obj) {
            Some(Object::Interrupt { intid }) => Ok(intid),
            _ => Err(KError::BadHandle),
        },
        Some(_) => Err(KError::PermissionDenied),
        None => Err(KError::BadHandle),
    }
}

/// Resolve an endpoint capability handle to its table id, requiring **receive**
/// rights. Shared by `EndpointBind` and `EndpointPending`, both of which answer the
/// question "is there something here for me to take" and neither of which a
/// send-only holder has any business asking.
fn resolve_receivable_endpoint(handle: u32) -> Result<usize, KError> {
    match sched::resolve_cap(handle) {
        Some(Cap::Endpoint { obj, recv: true, .. }) => match obj::get(obj) {
            Some(Object::Endpoint { id }) => Ok(id),
            _ => Err(KError::BadHandle),
        },
        Some(_) => Err(KError::PermissionDenied),
        None => Err(KError::BadHandle),
    }
}

/// Resolve a notification capability handle to its table id.
fn resolve_notification(handle: u32) -> Result<usize, KError> {
    match sched::resolve_cap(handle) {
        Some(Cap::Notification { obj }) => match obj::get(obj) {
            Some(Object::Notification { id }) => Ok(id),
            _ => Err(KError::BadHandle),
        },
        Some(_) => Err(KError::PermissionDenied),
        None => Err(KError::BadHandle),
    }
}

/// The `WaitAny` syscall: resolve an array of notification handles from the
/// caller's memory, wait for the first to fire, and return its index.
///
/// The handles are **copied out of user memory before anything is resolved**, and
/// then resolved into table ids in one pass. Both halves matter: reading the array
/// twice would let a caller change it between the check and the use, and resolving
/// lazily inside the wait loop would mean touching user memory while parked.
fn wait_any(ptr: u64, count: usize, deadline_ns: u64) -> isize {
    /// Ceiling on how many sources one wait may name. Generous for an event loop's
    /// real fan-out, and small enough to live on the kernel stack.
    const MAX_WAIT_HANDLES: usize = 16;

    if count == 0 || count > MAX_WAIT_HANDLES {
        return KError::InvalidArgument.as_raw();
    }
    let bytes = count * size_of::<u32>();
    // Handles are 4-byte values; require the array to be aligned to one, and every
    // byte of it to be mapped EL0-readable in the caller's own tables.
    if !ptr.is_multiple_of(size_of::<u32>() as u64) || !sched::current_range_ok(ptr, bytes, false) {
        return KError::InvalidArgument.as_raw();
    }
    let mut raw = [0u8; MAX_WAIT_HANDLES * size_of::<u32>()];
    // SAFETY: the caller's space is active and the walk above confirmed all
    // `bytes` bytes are mapped EL0-readable; `bytes <= raw.len()`.
    unsafe { staros_arch_aarch64::usercopy::copy_from_user(&mut raw[..bytes], ptr) };

    let mut ids = [0usize; MAX_WAIT_HANDLES];
    let (words, _) = raw[..bytes].as_chunks::<4>();
    for (slot, chunk) in ids[..count].iter_mut().zip(words) {
        let handle = u32::from_le_bytes(*chunk);
        match resolve_notification(handle) {
            Ok(id) => *slot = id,
            Err(e) => return e.as_raw(),
        }
    }

    // Zero means "no deadline" rather than "a deadline at time zero": a deadline of
    // zero is always already past, so honouring it literally would turn every such
    // call into a non-blocking poll — a plausible-looking wait that never waits.
    let deadline = (deadline_ns != 0).then_some(deadline_ns);
    match notify::wait_any(&ids[..count], deadline) {
        Some(index) => index as isize,
        None => KError::WouldBlock.as_raw(),
    }
}

/// Returns `true` if the kernel may dereference a caller-supplied pointer to
/// `len` bytes: 8-byte aligned, and every page of it actually mapped for the
/// calling task with the rights this access needs (`write` for a store).
///
/// This walks the caller's page tables. It used to be a range check against a
/// hard-coded window size, which was only ever *nearly* true — a user could name
/// an unmapped address inside the window and the kernel would take a data abort
/// at EL1 trying to honour it. With the EL0 window now a sparse 48-bit space,
/// "inside the window" stopped meaning anything at all, so the check follows the
/// same tables the hardware would.
fn user_ptr_ok(ptr: u64, len: usize, write: bool) -> bool {
    ptr.is_multiple_of(8) && sched::current_range_ok(ptr, len, write)
}

/// Read a [`Message`] from a caller-supplied user pointer, or `None` if the
/// pointer fails validation.
fn read_user_msg(ptr: u64) -> Option<Message> {
    if !user_ptr_ok(ptr, size_of::<Message>(), false) {
        return None;
    }
    // Read the message out of EL0 memory with unprivileged loads (PAN-safe) rather
    // than a direct EL1 dereference. `Message` is `repr(C)` and `Copy` with no
    // invalid bit patterns, so reconstituting it from its bytes is sound.
    let mut slot = core::mem::MaybeUninit::<Message>::uninit();
    // SAFETY: the caller's active space has every page confirmed mapped/EL0-readable
    // by the walk above; the byte slice spans exactly one `Message` in the slot.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(slot.as_mut_ptr().cast::<u8>(), size_of::<Message>());
        staros_arch_aarch64::usercopy::copy_from_user(dst, ptr);
        Some(slot.assume_init())
    }
}

/// Write a [`Message`] to a caller-supplied user pointer; returns `false` if the
/// pointer fails validation.
fn write_user_msg(ptr: u64, msg: Message) -> bool {
    if !user_ptr_ok(ptr, size_of::<Message>(), true) {
        return false;
    }
    // Write the message into EL0 memory with unprivileged stores (PAN-safe). Viewing
    // the `repr(C)` `Copy` value as bytes is sound; `STTR` honours the EL0 write
    // permission the walk confirmed.
    // SAFETY: `&msg` is a live `Message`; the byte slice spans exactly its size, and
    // the destination range is confirmed mapped/EL0-writable.
    unsafe {
        let src = core::slice::from_raw_parts((&raw const msg).cast::<u8>(), size_of::<Message>());
        staros_arch_aarch64::usercopy::copy_to_user(ptr, src);
    }
    true
}

/// Issue a syscall from kernel context — a self-test of the SVC path using the
/// same convention userspace will: number in `x8`, arguments in `x0`..=`x5`,
/// result returned in `x0`.
///
/// # Safety
/// Requires the EL1 exception vectors to be installed. The trap trampoline
/// preserves every general-purpose register except `x0` (the result), so this
/// declares no additional clobbers.
pub unsafe fn invoke(number: usize, arg0: u64) -> isize {
    let ret: isize;
    // SAFETY: `svc #0` raises a synchronous exception handled by our installed
    // EL1 vectors, which return the dispatcher's value in x0.
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number,
            inout("x0") arg0 => ret,
            options(nostack),
        );
    }
    ret
}
