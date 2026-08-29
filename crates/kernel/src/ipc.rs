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


use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use staros_abi::error::KError;
use staros_arch_aarch64::boot;
use staros_ipc::Message;

use crate::cap::Cap;
use crate::sched;
use crate::sync::SpinLock;

/// Number of endpoints the kernel exposes. Two carry the client<->server request
/// and reply; two more carry the device manager's grants to the driver and the
/// server; one is the contention endpoint several senders hammer at once; two
/// carry the display protocol's commit and its reply, and six more do the same for
/// its other three clients; four are two file servers' request/reply pairs (one per
/// client — see `kmain`); one carries decoded input events out of the input driver
/// to the display server; the last four carry them on from there to whichever
/// display client has the focus. A real system allocates them dynamically.
///
/// A reply endpoint per client rather than one shared by all of them is not
/// bookkeeping: two clients receiving on one endpoint means either may take the
/// other's answer, and the symptom is a program that acts on a reply to a question
/// it never asked.
///
/// Ids used to be a *shared numbering* between this table and whoever minted the
/// objects in `main`, and that seam bit twice.
///
/// Once by collision: giving the display endpoint id 4 put its traffic into the
/// contention test's endpoint, and the display server spent its life rejecting
/// storm messages it had no business seeing. Once by overrun: when the QML program
/// was given a file server of its own, its pair landed past the end of a
/// fixed-size table. Nothing refused the ids — `obj::create` made the objects, the
/// capabilities were installed, the server started and announced itself — and then
/// `Recv` returned `InvalidArgument` on an endpoint with no slot. The console said
/// `[fssrv] receive failed`, naming neither the endpoint, nor the id, nor the
/// constant that was too small.
///
/// Both are gone because **nobody picks a number any more**. [`allocate`] hands one
/// out, the table grows to fit, and a caller cannot name an endpoint that does not
/// exist or one that belongs to somebody else. The comment above is kept because
/// the two failures are the reason this is not a fixed array.
/// Whether `id` names a live endpoint.
///
/// Still checked, and not only for tidiness: an endpoint object outlives the slot
/// numbering only if something hands `Object::Endpoint` an id it did not get from
/// [`allocate`], and this is where that would be caught rather than at a `Recv` in
/// a service that has no idea what an id is.
#[must_use]
pub fn endpoint_exists(id: usize) -> bool {
    IPC.lock().get(id).is_some_and(|e| e.allocated)
}

/// Take a free endpoint slot and return its id, growing the table if needed.
///
/// This is the whole of the fix: an id comes from here or it does not exist. Slots
/// are reused once freed, and the table only ever grows to the high-water mark of
/// endpoints alive at once.
pub fn allocate() -> Option<usize> {
    let mut table = IPC.lock();
    if let Some(id) = table.iter().position(|e| !e.allocated) {
        table[id] = Endpoint::new();
        table[id].allocated = true;
        return Some(id);
    }
    if table.try_reserve(1).is_err() {
        return None;
    }
    let mut endpoint = Endpoint::new();
    endpoint.allocated = true;
    table.push(endpoint);
    Some(table.len() - 1)
}

/// Which endpoint the IPC contention test uses (see [`storm_stats`]), once it has
/// been allocated one.
///
/// Two things are special about it, both for the same reason — it is hammered
/// hundreds of times by several tasks at once, where the others carry a handful of
/// messages each. It does not log blocked sends (that would bury the console), and
/// it is the only endpoint whose traffic is counted per core.
///
/// A variable rather than a constant now, because the id is whatever [`allocate`]
/// gave: hard-coding it here is exactly the shared numbering this file no longer
/// has. `usize::MAX` means "no storm endpoint", which no allocated id can be.
static STORM_EP: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Name `id` as the contention test's endpoint. Called once, by whoever creates it.
pub fn set_storm_endpoint(id: usize) {
    STORM_EP.store(id, Ordering::Relaxed);
}

/// Whether `ep` is the contention test's endpoint.
fn is_storm(ep: usize) -> bool {
    STORM_EP.load(Ordering::Relaxed) == ep
}
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
    /// Whether this slot has been handed out by [`allocate`]. A slot that has not
    /// is not an endpoint with no messages — it is not an endpoint, and a send to it
    /// must fail rather than sit in a ring nobody will ever read.
    allocated: bool,
    ring: [KMessage; RING_CAP],
    head: usize,
    len: usize,
    recv_waiters: [usize; MAX_WAITERS],
    n_recv: usize,
    send_waiters: [(usize, KMessage); MAX_WAITERS],
    n_send: usize,
    /// A notification signalled whenever a message arrives here, if a holder of
    /// this endpoint's receive rights asked for one. See [`bind_notify`].
    notify: Option<usize>,
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            allocated: false,
            ring: [KMessage::empty(); RING_CAP],
            head: 0,
            len: 0,
            recv_waiters: [0; MAX_WAITERS],
            n_recv: 0,
            send_waiters: [(0, KMessage::empty()); MAX_WAITERS],
            n_send: 0,
            notify: None,
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
/// It grows on demand rather than being a fixed array, for the same reason the
/// object table does: how many endpoints a system needs is a property of what runs
/// on it, and a table sized here would refuse the first thing that needed one more.
static IPC: SpinLock<Vec<Endpoint>> = SpinLock::new(Vec::new());

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
    send_inner(ep, km, true)
}

/// Send `km` to endpoint `ep` without ever parking: [`KError::WouldBlock`] when
/// there is no room, rather than waiting for one.
///
/// Backs `SendNoWait`, which exists for the one caller shape that must not block —
/// a server pushing unsolicited events to a client. See the syscall's own
/// documentation for what happened without it.
pub fn send_nowait(ep: usize, km: KMessage) -> isize {
    send_inner(ep, km, false)
}

fn send_inner(ep: usize, km: KMessage, may_block: bool) -> isize {
    if !endpoint_exists(ep) {
        return KError::InvalidArgument.as_raw();
    }
    let me = sched::current_id();
    if is_storm(ep) {
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
        /// No room, and the caller asked not to wait.
        WouldBlock,
    }
    let (action, bound) = {
        // The guard's scope is this block: it is released before any of the
        // actions below can context switch.
        let mut table = IPC.lock();
        let e = &mut table[ep];
        let bound = e.notify;
        let action = if let Some(w) = e.pop_recv_waiter() {
            Action::Deliver(w)
        } else if e.push_msg(km) {
            Action::Buffered
        } else if !may_block {
            // Decided inside the lock, with the rest: asking whether there is room
            // and then sending is two decisions about a queue other cores are using,
            // and the answer to the first stops being true between them.
            Action::WouldBlock
        } else if e.push_send_waiter(me, km) {
            Action::Block
        } else {
            Action::Full
        };
        (action, bound)
    };
    // A bound notification is signalled for every send that placed a message,
    // including one handed straight to a blocked receiver. Signalling in that case
    // is redundant — nobody polling was waiting for it — and it costs one spurious
    // wake-up, after which the poller re-asks [`pending`] and goes back to sleep.
    // The other way round costs a program that sleeps for ever holding a message,
    // so the error is deliberately made on the noisy side.
    if matches!(action, Action::Deliver(_) | Action::Buffered) {
        if let Some(id) = bound {
            crate::notify::signal(id);
        }
    }
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
            if !is_storm(ep) {
                log(format_args!("[ipc] task {me} send blocked: ep{ep} ring full"));
            }
            sched::block_current();
            if !is_storm(ep) {
                log(format_args!("[ipc] task {me} send resumed"));
            }
            0
        }
        Action::Full => KError::OutOfResources.as_raw(),
        // No log line. This is the ordinary outcome for an event nobody is reading,
        // it happens once per pointer movement, and a server that counts its drops
        // has better evidence than a kernel that narrates them.
        Action::WouldBlock => KError::WouldBlock.as_raw(),
    }
}

/// Receive from endpoint `ep`: return a buffered message, or block until one
/// arrives. Draining a slot wakes a blocked sender (moving its message into the
/// freed slot). Returns the [`KMessage`] or a negative [`KError`].
/// Receive from endpoint `ep`: return a buffered message, or wait for one until
/// `deadline_ns` on the monotonic clock. Draining a slot wakes a blocked sender
/// (moving its message into the freed slot).
///
/// `None` waits for ever, which is what a server that has nothing else to do
/// should do. A deadline is for the callers that do: a client waiting for an
/// answer that may never come, and a service that must notice a peer went away.
/// Without this the only way to wait with a bound was to bind a notification to
/// the endpoint and use `WaitAny`, which is three syscalls and a notification per
/// endpoint to express "wait, but not forever".
///
/// Absolute, like `SleepUntil` and `WaitAny`, and for the same reason: a duration
/// is measured from whenever the call happens to run, so a caller preempted
/// between computing it and asking for it waits longer than it asked, and cannot
/// tell.
///
/// The timeout path has to **remove this task from the endpoint's receiver queue**.
/// A waiter left behind is worse than a leak: the next sender delivers straight to
/// it, `unblock` finds a task that is not waiting for anything, and the message is
/// gone — taken by nobody, from a queue nobody can see.
pub fn recv_until(ep: usize, deadline_ns: Option<u64>) -> Result<KMessage, KError> {
    if !endpoint_exists(ep) {
        return Err(KError::InvalidArgument);
    }
    let me = sched::current_id();
    if is_storm(ep) {
        count(&STORM_RECVS);
    }

    enum Action {
        Got(KMessage),
        WakeSender(KMessage, usize),
        Block,
        Full,
    }
    let (action, bound) = {
        // As in `send`: the lock does not outlive the decision.
        let mut table = IPC.lock();
        let e = &mut table[ep];
        let bound = e.notify;
        let action = match e.pop_msg() {
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
        };
        (action, bound)
    };
    match action {
        Action::Got(km) => Ok(km),
        Action::WakeSender(km, tid) => {
            // A message just moved into the ring from a sender that had been
            // blocked. That is an arrival like any other, and the only one a
            // *sender* cannot announce: it happened inside this receive.
            if let Some(id) = bound {
                crate::notify::signal(id);
            }
            sched::unblock(tid);
            Ok(km)
        }
        Action::Block => match deadline_ns {
            None => Ok(sched::block_for_message()),
            Some(deadline) => {
                sched::block_until(Some(deadline));
                // Woken by a sender, or by the clock. The mailbox is the only
                // honest way to tell: a message was put there *before* this task
                // was made runnable, so its presence means delivery happened and
                // its absence means the deadline won. Asking the clock instead
                // would race — a sender can deliver a microsecond before the
                // deadline the clock has already passed.
                match sched::take_mailbox() {
                    Some(km) => Ok(km),
                    None => {
                        cancel_recv_waiter(ep, me);
                        Err(KError::WouldBlock)
                    }
                }
            }
        },
        Action::Full => Err(KError::OutOfResources),
    }
}

/// Take `task` out of `ep`'s queue of blocked receivers, if it is there.
///
/// Only the timeout path needs this, and it needs it absolutely: a receiver that
/// stopped waiting but stayed in the queue is where the next message goes, and it
/// goes nowhere.
fn cancel_recv_waiter(ep: usize, task: usize) {
    let mut table = IPC.lock();
    let Some(e) = table.get_mut(ep) else { return };
    if let Some(at) = e.recv_waiters[..e.n_recv].iter().position(|&t| t == task) {
        e.recv_waiters.copy_within(at + 1..e.n_recv, at);
        e.n_recv -= 1;
    }
}

/// Bind notification `notif` to endpoint `ep`, so every message that arrives there
/// signals it — or unbind, with `None`. Returns `false` if `ep` names no endpoint.
///
/// One notification per endpoint, last binding wins. A list would let two event
/// loops watch one endpoint, which reads as a feature and is a race: both wake, one
/// takes the message, and the other has been told about a message that is no longer
/// there. With one binding the surprise is at least confined to whoever asked for
/// it, and a server that wants to share an endpoint has to say how.
///
/// Unbinding matters because notification slots are reused. A program that closes
/// an endpoint descriptor and leaves the binding behind has the kernel signalling a
/// slot that now belongs to something else — and the new owner is woken for a
/// connection it has never heard of.
pub fn bind_notify(ep: usize, notif: Option<usize>) -> bool {
    let mut table = IPC.lock();
    match table.get_mut(ep) {
        Some(e) => {
            e.notify = notif;
            true
        }
        None => false,
    }
}

/// How many messages are buffered at endpoint `ep` right now.
///
/// Blocked senders are deliberately not counted. Their messages are not at the
/// endpoint yet and a receive will not return one directly — it returns a buffered
/// message and only then lets a sender deposit. Counting them would report a
/// readiness that the very next receive cannot satisfy, and the ring is full
/// whenever a sender is blocked, so the count is already non-zero in every case
/// where it would have mattered.
#[must_use]
pub fn pending(ep: usize) -> usize {
    IPC.lock().get(ep).map_or(0, |e| e.len)
}

/// Where task `task` is waiting: `(endpoint, is_send)`, or `None` if it is not on
/// any endpoint's wait queue.
///
/// The scheduler can say a task is `Blocked`; it cannot say *what for*, because the
/// wait queues live here. The difference matters the first time a program stalls
/// rather than a server: a client parked on its own reply endpoint is waiting for an
/// answer that a server owes it, and a client parked on its event endpoint is
/// waiting for input that will never come — the same word, two different defects.
#[must_use]
pub fn waiting_on(task: usize) -> Option<(usize, bool)> {
    let ipc = IPC.lock();
    for (ep, e) in ipc.iter().enumerate() {
        if e.recv_waiters[..e.n_recv].contains(&task) {
            return Some((ep, false));
        }
        if e.send_waiters[..e.n_send].iter().any(|&(t, _)| t == task) {
            return Some((ep, true));
        }
    }
    None
}

/// Emit a short kernel diagnostic (used to make the blocking-send path visible).
fn log(args: core::fmt::Arguments) {
    crate::console::println(args);
}
