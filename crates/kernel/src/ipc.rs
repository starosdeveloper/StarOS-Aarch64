//! Synchronous message-passing endpoints — the microkernel's core mechanism.
//!
//! In a microkernel the kernel keeps almost no policy; what it *must* provide is
//! a way for isolated tasks — which share no memory — to communicate. That is an
//! endpoint: a rendezvous point a task can [`send`] to and [`recv`] from. The
//! message is copied *through the kernel*; neither side can touch the other's
//! memory.
//!
//! A message may also **transfer a capability**: the sender names one of its own
//! capabilities in `Message.cap`, the kernel carries the *resolved* [`Cap`]
//! (handles are per-task and meaningless across the boundary), and the receiver
//! has it installed into its table under a fresh handle. That is how one service
//! hands another the authority to talk to it — dynamic capability granting.
//!
//! Queueing policy: each endpoint has a bounded ring plus wait queues for blocked
//! receivers *and* blocked senders. [`recv`] returns a buffered message or blocks;
//! [`send`] delivers to a waiting receiver, else buffers, else — if the ring is
//! full — blocks the sender until a receiver drains a slot. The `Send`/`Recv`
//! syscalls (see [`crate::syscall`]) adapt the user ABI onto these two functions.


use core::sync::atomic::{AtomicU64, Ordering};

use staros_abi::error::KError;
use staros_arch_aarch64::boot;
use staros_ipc::Message;

use crate::cap::Cap;
use crate::sched;
use crate::sync::SpinLock;

/// Number of endpoints the kernel exposes. Two carry the client<->server request
/// and reply; two more carry the device manager's grants to the driver and the
/// server; one is the contention endpoint several senders hammer at once; two
/// carry the display protocol's commit and its reply; the last two carry the file
/// server's requests and replies. A real system allocates them dynamically.
///
/// The ids are a *shared numbering* between this table and whoever creates the
/// objects in `main`, which is exactly the kind of seam that bites: giving the
/// display endpoint id 4 put its traffic into [`STORM_EP`], and the display server
/// spent its life rejecting storm messages it had no business seeing.
const NUM_ENDPOINTS: usize = 10;

/// The endpoint the IPC contention test uses (see [`storm_stats`]).
///
/// Two things are special about it, both for the same reason — it is hammered
/// hundreds of times by several tasks at once, where the others carry a handful of
/// messages each. It does not log blocked sends (that would bury the console), and
/// it is the only endpoint whose traffic is counted per core.
pub const STORM_EP: usize = 4;
/// Capacity of an endpoint's pending-message ring. Deliberately tiny so the
/// blocking-send path is exercised (the demo client sends three requests into a
/// two-slot ring) rather than hidden behind a large buffer.
const RING_CAP: usize = 2;
/// Maximum tasks that can be blocked (each direction) on one endpoint at once.
const MAX_WAITERS: usize = 4;

/// A message as the kernel carries it: the user-visible [`Message`] plus the
/// resolved capability being transferred, if any. `Copy`, so no allocation.
#[derive(Clone, Copy)]
pub struct KMessage {
    /// The bytes delivered to the receiver's buffer.
    pub msg: Message,
    /// A capability transferred with the message, already resolved from the
    /// sender's table into a kernel object; installed into the receiver's table
    /// on delivery. `None` if the message transfers no capability.
    pub cap: Option<Cap>,
}

impl KMessage {
    const fn empty() -> Self {
        Self {
            msg: Message::new(0),
            cap: None,
        }
    }
}

/// A rendezvous point: a bounded message ring plus queues of blocked receivers
/// and blocked senders (the latter carrying the message they could not deposit).
struct Endpoint {
    ring: [KMessage; RING_CAP],
    head: usize,
    len: usize,
    recv_waiters: [usize; MAX_WAITERS],
    n_recv: usize,
    send_waiters: [(usize, KMessage); MAX_WAITERS],
    n_send: usize,
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            ring: [KMessage::empty(); RING_CAP],
            head: 0,
            len: 0,
            recv_waiters: [0; MAX_WAITERS],
            n_recv: 0,
            send_waiters: [(0, KMessage::empty()); MAX_WAITERS],
            n_send: 0,
        }
    }

    fn push_msg(&mut self, km: KMessage) -> bool {
        if self.len == RING_CAP {
            return false;
        }
        let tail = (self.head + self.len) % RING_CAP;
        self.ring[tail] = km;
        self.len += 1;
        true
    }

    fn pop_msg(&mut self) -> Option<KMessage> {
        if self.len == 0 {
            return None;
        }
        let km = self.ring[self.head];
        self.head = (self.head + 1) % RING_CAP;
        self.len -= 1;
        Some(km)
    }

    fn push_recv_waiter(&mut self, task: usize) -> bool {
        if self.n_recv == MAX_WAITERS {
            return false;
        }
        self.recv_waiters[self.n_recv] = task;
        self.n_recv += 1;
        true
    }

    fn pop_recv_waiter(&mut self) -> Option<usize> {
        if self.n_recv == 0 {
            return None;
        }
        let task = self.recv_waiters[0];
        self.recv_waiters.copy_within(1..self.n_recv, 0);
        self.n_recv -= 1;
        Some(task)
    }

    fn push_send_waiter(&mut self, task: usize, km: KMessage) -> bool {
        if self.n_send == MAX_WAITERS {
            return false;
        }
        self.send_waiters[self.n_send] = (task, km);
        self.n_send += 1;
        true
    }

    fn pop_send_waiter(&mut self) -> Option<(usize, KMessage)> {
        if self.n_send == 0 {
            return None;
        }
        let waiter = self.send_waiters[0];
        self.send_waiters.copy_within(1..self.n_send, 0);
        self.n_send -= 1;
        Some(waiter)
    }
}

/// The endpoint table.
///
/// The lock is held for the *decision* only. `send` and `recv` below already
/// worked that way — decide under a short borrow, act after dropping it —
/// because blocking performs a context switch and must not alias the endpoint.
/// That discipline is what makes a lock here safe rather than a deadlock: a guard
/// held across `block_current` would leave the lock owned by a task that is no
/// longer running, and the next core to want an endpoint would wait for it
/// forever.
static IPC: SpinLock<[Endpoint; NUM_ENDPOINTS]> =
    SpinLock::new([const { Endpoint::new() }; NUM_ENDPOINTS]);

/// Sends and receives on [`STORM_EP`], counted by the core that executed them.
///
/// `smp::race_test` proves the kernel's *lock* excludes across cores on a counter
/// in kernel memory. It says nothing about IPC: the endpoint has its own ring,
/// its own wait queues, and a block/wake path that runs a context switch in the
/// middle. These counters are what make the contention real rather than assumed —
/// if the senders ran one after another on one core, or the receiver only ever ran
/// where the senders did, the per-core spread shows it and the test says so.
static STORM_SENDS: [AtomicU64; boot::MAX_CPUS] = [const { AtomicU64::new(0) }; boot::MAX_CPUS];
static STORM_RECVS: [AtomicU64; boot::MAX_CPUS] = [const { AtomicU64::new(0) }; boot::MAX_CPUS];

/// Record one storm operation against the core currently executing it.
fn count(table: &[AtomicU64; boot::MAX_CPUS]) {
    let cpu = boot::cpu_id() as usize;
    if cpu < boot::MAX_CPUS {
        table[cpu].fetch_add(1, Ordering::Relaxed);
    }
}

/// Per-core `(sends, receives)` on the contention endpoint, plus how many distinct
/// cores took part in each direction.
#[must_use]
pub fn storm_stats() -> ([u64; boot::MAX_CPUS], [u64; boot::MAX_CPUS], u32, u32) {
    let mut sends = [0u64; boot::MAX_CPUS];
    let mut recvs = [0u64; boot::MAX_CPUS];
    let (mut send_cores, mut recv_cores) = (0, 0);
    for cpu in 0..boot::MAX_CPUS {
        sends[cpu] = STORM_SENDS[cpu].load(Ordering::Relaxed);
        recvs[cpu] = STORM_RECVS[cpu].load(Ordering::Relaxed);
        send_cores += u32::from(sends[cpu] > 0);
        recv_cores += u32::from(recvs[cpu] > 0);
    }
    (sends, recvs, send_cores, recv_cores)
}

/// Send `km` to endpoint `ep`. Delivers straight to a blocked receiver, else
/// buffers it, else — if the ring is full — blocks the caller until a receiver
/// drains a slot. Returns `0` on success (possibly after blocking) or a negative
/// [`KError`].
pub fn send(ep: usize, km: KMessage) -> isize {
    if ep >= NUM_ENDPOINTS {
        return KError::InvalidArgument.as_raw();
    }
    let me = sched::current_id();
    if ep == STORM_EP {
        // Count on the core that is *entering* the send: after a blocking send the
        // task may resume elsewhere, and the question this answers is which core
        // ran the operation, not which one finished it.
        count(&STORM_SENDS);
    }

    // Decide what to do under a short borrow, then act after dropping it — the
    // block path performs a context switch that must not alias the endpoint.
    enum Action {
        Deliver(usize),
        Buffered,
        Block,
        Full,
    }
    let action = {
        // The guard's scope is this block: it is released before any of the
        // actions below can context switch.
        let mut table = IPC.lock();
        let e = &mut table[ep];
        if let Some(w) = e.pop_recv_waiter() {
            Action::Deliver(w)
        } else if e.push_msg(km) {
            Action::Buffered
        } else if e.push_send_waiter(me, km) {
            Action::Block
        } else {
            Action::Full
        }
    };
    match action {
        Action::Deliver(w) => {
            sched::deliver(w, km);
            0
        }
        Action::Buffered => 0,
        Action::Block => {
            // The storm endpoint blocks hundreds of times by design; logging each
            // one would drown every other line in the boot output. Its evidence is
            // the per-core tally, not a narrative.
            if ep != STORM_EP {
                log(format_args!("[ipc] task {me} send blocked: ep{ep} ring full"));
            }
            sched::block_current();
            if ep != STORM_EP {
                log(format_args!("[ipc] task {me} send resumed"));
            }
            0
        }
        Action::Full => KError::OutOfResources.as_raw(),
    }
}

/// Receive from endpoint `ep`: return a buffered message, or block until one
/// arrives. Draining a slot wakes a blocked sender (moving its message into the
/// freed slot). Returns the [`KMessage`] or a negative [`KError`].
pub fn recv(ep: usize) -> Result<KMessage, KError> {
    if ep >= NUM_ENDPOINTS {
        return Err(KError::InvalidArgument);
    }
    let me = sched::current_id();
    if ep == STORM_EP {
        count(&STORM_RECVS);
    }

    enum Action {
        Got(KMessage),
        WakeSender(KMessage, usize),
        Block,
        Full,
    }
    let action = {
        // As in `send`: the lock does not outlive the decision.
        let mut table = IPC.lock();
        let e = &mut table[ep];
        match e.pop_msg() {
            Some(km) => match e.pop_send_waiter() {
                // We freed a slot; let a blocked sender deposit its message.
                Some((tid, skm)) => {
                    e.push_msg(skm);
                    Action::WakeSender(km, tid)
                }
                None => Action::Got(km),
            },
            None if e.push_recv_waiter(me) => Action::Block,
            None => Action::Full,
        }
    };
    match action {
        Action::Got(km) => Ok(km),
        Action::WakeSender(km, tid) => {
            sched::unblock(tid);
            Ok(km)
        }
        Action::Block => Ok(sched::block_for_message()),
        Action::Full => Err(KError::OutOfResources),
    }
}

/// Emit a short kernel diagnostic (used to make the blocking-send path visible).
fn log(args: core::fmt::Arguments) {
    crate::console::println(args);
}
