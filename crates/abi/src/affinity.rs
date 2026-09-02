//! CPU affinity: which cores a task may run on, and which one should take it.
//!
//! This is the *policy* half of the scheduler's picker, cut out of the kernel for
//! the reason every other pure crate here was cut out: the bugs live in the
//! arithmetic, not in the hardware. A round-robin scan that wraps one slot short,
//! a preference pass that falls through to the wrong candidate, a mask that admits
//! core 64 — none of those announce themselves on a boot log. They show up as "the
//! pinned task ran somewhere else, sometimes", which is the hardest class of defect
//! this tree has. Here they are ordinary unit tests on the host.
//!
//! The kernel keeps the state (which slot is `Ready`, which core each task last
//! ran on) and passes it in as two predicates. Nothing in this module knows what a
//! task is.

/// An affinity mask: bit `n` set means "may run on core `n`".
///
/// A `u64` and not a growable set, deliberately. `boot::MAX_CPUS` is 8 today and
/// the largest machine this kernel is aimed at has eight cores; a mask that fits
/// in a register is one a syscall can carry in `x0` without a pointer, and a
/// pointer is what would make setting an affinity fallible for a reason that has
/// nothing to do with affinity.
pub type Mask = u64;

/// The mask that permits every core — what a task is born with.
pub const ALL: Mask = u64::MAX;

/// Whether `mask` permits core `cpu`.
///
/// A core index at or beyond 64 is permitted by *no* mask rather than by all of
/// them. Shifting by 64 or more is undefined in Rust and would panic in debug, so
/// the bound is checked rather than assumed — and "no" is the safe answer, because
/// the alternative is a task running on a core it was pinned away from.
#[must_use]
pub const fn allows(mask: Mask, cpu: usize) -> bool {
    cpu < Mask::BITS as usize && mask & (1 << cpu) != 0
}

/// A single-core mask, or [`ALL`] for a core index this mask type cannot name.
#[must_use]
pub const fn only(cpu: usize) -> Mask {
    if cpu < Mask::BITS as usize {
        1 << cpu
    } else {
        ALL
    }
}

/// The `last_cpu` of a task that has never been scheduled anywhere.
pub const NO_HOME: usize = usize::MAX;

/// Whether core `cpu` is the natural home of a task whose last core was `last_cpu`
/// — the locality preference, as one comparison.
///
/// **A task that has never run is at home on every core**, and that clause is the
/// whole reason this is a named function rather than `last_cpu == cpu` written
/// inline. Without it a fresh task is invisible to the preference pass and reachable
/// only by the fall-through — which means it runs only at an instant when *no*
/// previously-scheduled task is runnable. On a busy machine that instant may not
/// arrive: a newly created thread waits, the parent waits on it with a deadline, and
/// the deadline is what expires.
///
/// It is also the right answer on the merits and not merely a patch. The preference
/// exists to keep a task near cache lines it left behind, and a task that has never
/// run has left none anywhere. There is nothing to be near, so there is nothing to
/// defer it for.
#[must_use]
pub const fn homed_here(last_cpu: usize, cpu: usize) -> bool {
    last_cpu == cpu || last_cpu == NO_HOME
}

/// Whether `mask` names at least one core in `online`.
///
/// The one check that makes `SetAffinity` an error rather than a way to disappear:
/// a mask naming only cores that never came up leaves the task permanently
/// unrunnable, and a task that is `Ready` for ever is indistinguishable — in the
/// task table, in the shutdown report, in every log line — from one that is
/// waiting for a message. Refusing at the syscall is the only point where the
/// caller can still be told.
#[must_use]
pub const fn satisfiable(mask: Mask, online: Mask) -> bool {
    mask & online != 0
}

/// Choose the next slot for core `cpu` out of `n` slots, scanning round-robin from
/// just after `from`, preferring a slot this core ran before.
///
/// `pickable(i)` answers "may core `cpu` run slot `i` right now" — the kernel folds
/// state, the on-cpu guard and the affinity mask into it. `sticky(i)` answers "did
/// this core run slot `i` last time", and is only ever consulted for slots
/// `pickable` already admits.
///
/// **Two passes, and the order is the whole policy.** The first pass takes a task
/// whose cache lines this core still holds; the second takes anything at all. The
/// second pass is what keeps stickiness from becoming starvation: a preference that
/// could not fall through would leave a core idle beside a runnable task, which is
/// worse than a cold cache by a wide margin. Nothing here *balances* — there is no
/// queue to balance, since every core scans the same array — it only decides which
/// of the runnable tasks a core reaches for first.
///
/// `from` is excluded from both passes — **by the range, not by the predicate**.
/// It is the task the caller is switching away from, and returning it would mean
/// `context_switch(p, p)`: a context saved into the very struct it is then restored
/// from, a switch counted that never happened, and — the part that matters on
/// several cores — a task left `on_cpu` while its successor clears the flag,
/// which is an invitation for a peer to run a task this core is already running.
///
/// The scan therefore runs over offsets `1..n`, which reaches every slot exactly
/// once and never wraps back to the start. An inclusive `1..=n` does wrap, and the
/// kernel's own scan used to: it was harmless only because every call site happened
/// to mark `from` unpickable before asking. That is a property of five callers
/// rather than of the scan, and it stopped being true the moment a sixth wanted to
/// leave a task `Ready` while looking for its successor.
#[must_use]
pub fn choose(
    n: usize,
    from: usize,
    pickable: impl Fn(usize) -> bool,
    sticky: impl Fn(usize) -> bool,
) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let order = |off: usize| (from + off) % n;
    (1..n)
        .map(order)
        .find(|&i| pickable(i) && sticky(i))
        .or_else(|| (1..n).map(order).find(|&i| pickable(i)))
}

/// Choose a slot for a core that is running nothing — the bootstrap and idle
/// loops, which have no task to scan from.
///
/// The same two passes as [`choose`], starting at slot 0 and considering every
/// slot including the first. It is a separate entry point rather than
/// `choose(n, n - 1, ..)` because "scan from nowhere" and "scan from the task I am
/// leaving" differ in exactly one slot, and that slot is the one an idle core must
/// be able to pick up.
#[must_use]
pub fn choose_first(
    n: usize,
    pickable: impl Fn(usize) -> bool,
    sticky: impl Fn(usize) -> bool,
) -> Option<usize> {
    (0..n)
        .find(|&i| pickable(i) && sticky(i))
        .or_else(|| (0..n).find(|&i| pickable(i)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_permits_every_representable_core() {
        for cpu in 0..64 {
            assert!(allows(ALL, cpu), "cpu {cpu}");
        }
    }

    #[test]
    fn a_core_beyond_the_mask_is_permitted_by_nothing() {
        // Not "permitted by everything": a shift of 64 is UB, and the safe answer
        // to an unnameable core is no.
        assert!(!allows(ALL, 64));
        assert!(!allows(ALL, 1_000));
        assert!(!allows(0, 64));
    }

    #[test]
    fn only_names_one_core() {
        assert_eq!(only(0), 0b1);
        assert_eq!(only(3), 0b1000);
        assert!(allows(only(3), 3));
        assert!(!allows(only(3), 2));
        // Unnameable: fall back to permitting everything rather than to permitting
        // nothing, which would be an unrunnable task built out of a typo.
        assert_eq!(only(64), ALL);
    }

    #[test]
    fn a_task_that_has_never_run_is_at_home_everywhere() {
        // The regression this function exists for. A fresh task belongs to no core,
        // and a preference pass that skipped it left it reachable only when nothing
        // that had already run was runnable — starvation, and on a single core the
        // smoke matrix caught it as a thread that never started.
        for cpu in 0..8 {
            assert!(homed_here(NO_HOME, cpu), "cpu {cpu}");
        }
        assert!(homed_here(2, 2));
        assert!(!homed_here(2, 3));
    }

    #[test]
    fn a_fresh_slot_is_reached_by_the_first_pass() {
        // Slot 2 has never run; slot 1 is this core's. Both are runnable. The fresh
        // one must be an ordinary candidate in the preference pass, so plain
        // round-robin order decides between them rather than one being deferred
        // behind the other indefinitely.
        let ready = [1usize, 2];
        let pickable = |i: usize| ready.contains(&i);
        let sticky = |i: usize| homed_here(if i == 1 { 0 } else { NO_HOME }, 0);
        assert_eq!(choose(4, 0, &pickable, &sticky), Some(1));
        assert_eq!(choose(4, 1, &pickable, &sticky), Some(2));
    }

    #[test]
    fn satisfiable_needs_one_online_core_in_common() {
        let online = 0b0011; // cores 0 and 1 came up
        assert!(satisfiable(0b0001, online));
        assert!(satisfiable(0b0010, online));
        assert!(satisfiable(0b1011, online));
        // Cores 2 and 3 are in the device tree and never arrived.
        assert!(!satisfiable(0b1100, online));
        assert!(!satisfiable(0, online));
    }

    /// `pickable` from a list of runnable slots, `sticky` from a list of slots this
    /// core ran last.
    fn preds<'a>(
        ready: &'a [usize],
        mine: &'a [usize],
    ) -> (impl Fn(usize) -> bool + 'a, impl Fn(usize) -> bool + 'a) {
        (
            move |i: usize| ready.contains(&i),
            move |i: usize| mine.contains(&i),
        )
    }

    #[test]
    fn round_robin_when_nothing_is_sticky() {
        let (pickable, sticky) = preds(&[0, 1, 2, 3], &[]);
        assert_eq!(choose(4, 0, &pickable, &sticky), Some(1));
        assert_eq!(choose(4, 1, &pickable, &sticky), Some(2));
        assert_eq!(choose(4, 3, &pickable, &sticky), Some(0));
    }

    #[test]
    fn the_slot_we_came_from_is_never_returned() {
        // Only slot 2 is runnable, and it is the one we are leaving: there is
        // nothing to switch to, and saying `Some(2)` would be a switch to self.
        // This is the case an inclusive wrap gets wrong, and the reason the
        // exclusion is in the range rather than in a caller's discipline.
        let (pickable, sticky) = preds(&[2], &[2]);
        assert_eq!(choose(4, 2, &pickable, &sticky), None);
        // Same slot, no stickiness: the second pass must not readmit it either.
        let (pickable, sticky) = preds(&[2], &[]);
        assert_eq!(choose(4, 2, &pickable, &sticky), None);
    }

    #[test]
    fn a_table_of_one_has_nowhere_to_switch() {
        // The degenerate shape of the same rule: one slot, and it is `from`.
        let (pickable, sticky) = preds(&[0], &[0]);
        assert_eq!(choose(1, 0, &pickable, &sticky), None);
    }

    #[test]
    fn a_sticky_slot_wins_over_an_earlier_one() {
        // Round-robin order from 0 is 1, 2, 3, 0. Slot 3 is ours; slots 1 and 2 are
        // runnable and belong to nobody. Without the preference pass this returns 1.
        let (pickable, sticky) = preds(&[1, 2, 3], &[3]);
        assert_eq!(choose(4, 0, &pickable, &sticky), Some(3));
    }

    #[test]
    fn preference_falls_through_rather_than_idling() {
        // Our slot (3) is not runnable. A preference that could not fall through
        // would return None and leave this core idle beside two runnable tasks.
        let (pickable, sticky) = preds(&[1, 2], &[3]);
        assert_eq!(choose(4, 0, &pickable, &sticky), Some(1));
    }

    #[test]
    fn sticky_is_only_consulted_for_pickable_slots() {
        // Slot 0 is ours but blocked; slot 2 is runnable and not ours.
        let (pickable, sticky) = preds(&[2], &[0]);
        assert_eq!(choose(4, 1, &pickable, &sticky), Some(2));
    }

    #[test]
    fn the_scan_wraps_the_whole_table_exactly_once() {
        // Only the slot immediately *before* `from` is runnable, which is the last
        // one a correct wrap reaches and the first one an off-by-one misses.
        let (pickable, sticky) = preds(&[2], &[]);
        assert_eq!(choose(4, 3, &pickable, &sticky), Some(2));
    }

    #[test]
    fn nothing_runnable_is_none() {
        let (pickable, sticky) = preds(&[], &[]);
        assert_eq!(choose(4, 0, &pickable, &sticky), None);
        assert_eq!(choose_first(4, &pickable, &sticky), None);
    }

    #[test]
    fn an_empty_table_is_none_rather_than_a_division_by_zero() {
        let (pickable, sticky) = preds(&[], &[]);
        assert_eq!(choose(0, 0, &pickable, &sticky), None);
        assert_eq!(choose_first(0, &pickable, &sticky), None);
    }

    #[test]
    fn an_idle_core_may_pick_slot_zero() {
        // The difference between `choose_first` and `choose`: slot 0 is excluded
        // from a scan that starts *from* slot 0, and must not be excluded from a
        // scan by a core that is running nothing.
        let (pickable, sticky) = preds(&[0], &[]);
        assert_eq!(choose(1, 0, &pickable, &sticky), None);
        assert_eq!(choose_first(1, &pickable, &sticky), Some(0));
    }

    #[test]
    fn an_idle_core_prefers_what_it_ran_before() {
        let (pickable, sticky) = preds(&[0, 1, 2], &[2]);
        assert_eq!(choose_first(3, &pickable, &sticky), Some(2));
    }
}
