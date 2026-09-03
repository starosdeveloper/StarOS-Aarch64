//! Scheduling classes: what a task is owed when several want the same core.
//!
//! [`affinity`](crate::affinity) answers *where* a task may run, and until this
//! module there was no answer to *in what order*. Every runnable task was equal:
//! the timer preempted them all at the same interval, and a display server with a
//! frame to finish waited behind a memory test with eight megabytes to walk. That
//! is the difference between a smooth frame and a merely average one, and it is
//! not a difference affinity can express — pinning the compositor to its own core
//! makes the machine smaller instead of making the compositor sooner.
//!
//! The policy here is **strict bands with a bounded escape**, and both halves are
//! load bearing:
//!
//! * Strict, because a preference that could be outbid is not a class. A
//!   [`Class::Latency`] task that is runnable is picked before a
//!   [`Class::Normal`] one that is runnable, on the same core, every time — and
//!   the ordering is applied *before* the locality preference, because a warm
//!   cache is worth less than being first.
//! * Bounded, because strict priority alone is a way to hang. A
//!   [`Class::Latency`] task that never blocks would own its core for ever and
//!   everything below it would stop, which is indistinguishable from a deadlock
//!   in every log this kernel prints. So one pick in every [`FAIRNESS_EVERY`],
//!   per core, runs the bands *upside down* and hands the core to the lowest
//!   class that has anything runnable.
//!
//! That second rule is also the answer to the authority question this module
//! would otherwise raise. There is no capability guarding a class: a task simply
//! declares its own, and nothing stops a memory test from calling itself
//! latency-sensitive. What bounds the damage is arithmetic rather than
//! permission — the worst a whole machine full of self-declared
//! [`Class::Latency`] tasks can take is `(FAIRNESS_EVERY - 1) / FAIRNESS_EVERY`
//! of each core, and the remainder still reaches the bottom band. A capability
//! would be a better answer and is not one this kernel has; a bound that holds
//! against a caller acting in bad faith is one it can have today.
//!
//! Like [`affinity`](crate::affinity), this is the policy cut out of the kernel
//! so that the arithmetic is testable on the host. Nothing here knows what a task
//! is; the kernel folds state into predicates and passes them in.

use crate::affinity;

/// What the scheduler owes a task relative to the others on its core.
///
/// The discriminants are the ABI values, not an internal encoding, and they start
/// at one so that zero can mean "tell me, change nothing" the way a zero mask does
/// in `SetAffinity`. Ordering is by discriminant and it is the whole policy:
/// `Latency < Normal < Bulk`, lower is picked first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u64)]
pub enum Class {
    /// Picked first, and expected not to be long: a display server compositing a
    /// frame, an input driver draining a queue. The class exists for work whose
    /// *deadline* matters more than its throughput.
    Latency = 1,
    /// What every task is born with, and where anything that has not thought
    /// about it belongs.
    Normal = 2,
    /// Picked when nothing above wants the core: a memory walk, a checksum, a
    /// test. Throughput work that would rather have the whole core later than a
    /// slice of it now.
    Bulk = 3,
}

/// The raw value that asks for the current class without setting one.
///
/// Deliberately the same shape as a zero affinity mask: the call that consumes a
/// value is the call that reports the current one, so a caller never has to guess
/// and then verify.
pub const QUERY: u64 = 0;

/// How many classes there are, for the arrays that count per-class.
pub const COUNT: usize = 3;

impl Class {
    /// A class from its ABI value, or `None` for anything else — including
    /// [`QUERY`], which is not a class and must not decode as one.
    #[must_use]
    pub const fn from_raw(n: u64) -> Option<Class> {
        match n {
            1 => Some(Class::Latency),
            2 => Some(Class::Normal),
            3 => Some(Class::Bulk),
            _ => None,
        }
    }

    /// This class's ABI value.
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self as u64
    }

    /// A dense index for per-class counters: `Latency` is 0.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize - 1
    }

    /// The name this class is reported under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Class::Latency => "latency",
            Class::Normal => "normal",
            Class::Bulk => "bulk",
        }
    }
}

/// One pick in every this many, per core, ignores the class ordering and serves
/// the lowest band instead.
///
/// Eight rather than a number tuned against a workload, and the reason is that no
/// workload here has been measured yet. What eight buys is a stated guarantee
/// with a stated cost: the bottom band is reached at least once in every eight
/// picks a core makes, and a latency-class task gives up an eighth of a busy core
/// to get that. Both numbers are visible in the shutdown report, so tuning this
/// later is a measurement rather than a guess.
pub const FAIRNESS_EVERY: u64 = 8;

/// Whether the pick numbered `picks` (per core, counting from zero) is the one
/// that serves the lowest band.
///
/// `FAIRNESS_EVERY - 1` and not `0`, so a core's *first* pick follows the classes.
/// A core whose very first decision were a fairness pick would hand the machine to
/// the bottom band at boot, which is the one moment there is nothing to be fair
/// about.
#[must_use]
pub const fn is_fair(picks: u64) -> bool {
    picks % FAIRNESS_EVERY == FAIRNESS_EVERY - 1
}

/// The bands, in the order the decision numbered `picks` should scan them.
///
/// An ordinary decision scans them in order of urgency. A fairness decision starts
/// at one of the bands *below the top* and falls through to the rest, with the top
/// band last — so the escape prefers anything at all to the band that has been
/// holding the core.
///
/// **Which lower band it starts at alternates, and that is not a refinement.** The
/// first version always started at [`Class::Bulk`], and the consequence was found
/// by hanging a boot: with a latency-class thread spinning on one core, every
/// fairness pick went to the bulk thread and the middle band never ran at all. The
/// thread waiting to stop the measurement was normal-class, so it never woke, so
/// the spinner never stopped. An escape that always serves the bottom does not
/// prevent starvation — it moves it one band up, where it is harder to see.
///
/// The fall-through is why the array is a full ordering rather than a single band.
/// A fairness pick on a core where nothing below the top is runnable must still
/// pick *something*; an escape that could return `None` would idle a core beside a
/// runnable task once in every [`FAIRNESS_EVERY`], which is a worse defect than the
/// starvation it was added to prevent.
#[must_use]
pub const fn bands(picks: u64) -> [Class; COUNT] {
    if !is_fair(picks) {
        [Class::Latency, Class::Normal, Class::Bulk]
    } else if (picks / FAIRNESS_EVERY).is_multiple_of(2) {
        [Class::Bulk, Class::Normal, Class::Latency]
    } else {
        [Class::Normal, Class::Bulk, Class::Latency]
    }
}

/// Choose the next slot for a core about to switch away from `from`, honouring
/// classes first and locality second.
///
/// One [`affinity::choose`] per band, which is what keeps the two policies from
/// growing a second copy of each other. Everything that scan promises still holds
/// inside a band — the round-robin order, the sticky pass, and above all the
/// exclusion of `from` — and the only thing added here is *which slots the scan is
/// allowed to see*.
///
/// The order of the two policies is the decision worth arguing about, and it is
/// classes first. Locality is an optimisation measured in cache misses; a class is
/// a statement about what the machine is for. A latency task deferred behind a
/// bulk task because the bulk task left warmer lines behind is a frame dropped to
/// save a cache fill, which is the trade backwards.
///
/// `picks` is the number of picks this core has already made; it selects the band
/// order via [`is_fair`] and nothing else.
#[must_use]
pub fn choose(
    n: usize,
    from: usize,
    picks: u64,
    pickable: impl Fn(usize) -> bool,
    sticky: impl Fn(usize) -> bool,
    class_of: impl Fn(usize) -> Class,
) -> Option<usize> {
    for band in bands(picks) {
        let found = affinity::choose(n, from, |i| pickable(i) && class_of(i) == band, &sticky);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Choose a slot for a core that is running nothing — the bootstrap and idle
/// loops — honouring classes first, by the same band scan as [`choose`].
#[must_use]
pub fn choose_first(
    n: usize,
    picks: u64,
    pickable: impl Fn(usize) -> bool,
    sticky: impl Fn(usize) -> bool,
    class_of: impl Fn(usize) -> Class,
) -> Option<usize> {
    for band in bands(picks) {
        let found = affinity::choose_first(n, |i| pickable(i) && class_of(i) == band, &sticky);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Whether a core that is *involuntarily* preempting `running` should actually
/// hand the core to `candidate`.
///
/// **Without this, [`choose`] cannot express a priority at all, and the first
/// measurement said so.** Two threads on one core, one latency and one bulk,
/// neither ever blocking: the split came out even. The picker was obeying its
/// bands perfectly and it made no difference, because a timer preempt asks "who
/// else may run" and [`affinity::choose`] excludes the task being switched away
/// from — so the only candidate on that core was the bulk thread every single
/// time. Ordering candidates is worth nothing when the incumbent is not one of
/// them. A priority is as much about *not* switching as about which task is
/// picked, and that half has to be said separately.
///
/// So a preempt that would hand the core to a strictly less urgent task is
/// declined, and the incumbent keeps the rest of its slice. Equal classes still
/// switch, because round-robin inside a band is the fairness there already is;
/// only a *downward* switch is refused.
///
/// `fair` overrides it, and that is the same escape [`is_fair`] drives everywhere
/// else. Without the override this function is exactly how a spinning
/// latency-class task takes a core for ever — declining every preempt is a much
/// more effective way to hang a machine than merely being picked first.
///
/// Involuntary only. `Yield` means "take the core from me", and a task that says
/// so must be believed whatever its class: the caller passes its own decision
/// there rather than asking this.
#[must_use]
pub const fn should_switch(running: Class, candidate: Class, fair: bool) -> bool {
    fair || (candidate as u64) <= (running as u64)
}

/// Whether any slot of a class strictly above `chosen` was pickable — that is,
/// whether this pick was an inversion.
///
/// **This function exists to disagree with [`choose`], and that is its whole
/// value.** On the ordered path it cannot: `choose` reaches a band only after the
/// bands above it came up empty, so asking afterwards whether one of them had
/// something is asking a question the picker has already answered. That makes it
/// useless as a check on itself and useful as a check on *the picker* — it is a
/// separate scan, written separately, and the claim is that the two agree.
///
/// The claim is exact rather than approximate: an inversion is possible only on a
/// fairness pick, so over a whole boot the number of inversions can never exceed
/// the number of fairness picks. Take the band loop out of [`choose`] and every
/// pick becomes eligible to invert, which is a number the shutdown report prints
/// and the smoke matrix forbids.
///
/// `pickable` must already exclude the slot the caller is switching away from —
/// that slot is runnable and higher-class and still not a candidate, and counting
/// it would make [`choose`]'s own correctness look like a violation.
#[must_use]
pub fn outranked(
    n: usize,
    chosen: Class,
    pickable: impl Fn(usize) -> bool,
    class_of: impl Fn(usize) -> Class,
) -> bool {
    (0..n).any(|i| class_of(i) < chosen && pickable(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pickable` from a list of runnable slots, `sticky` from a list this core
    /// ran last, `class_of` from a table indexed by slot.
    fn preds<'a>(
        ready: &'a [usize],
        mine: &'a [usize],
        classes: &'a [Class],
    ) -> (
        impl Fn(usize) -> bool + 'a,
        impl Fn(usize) -> bool + 'a,
        impl Fn(usize) -> Class + 'a,
    ) {
        (
            move |i: usize| ready.contains(&i),
            move |i: usize| mine.contains(&i),
            move |i: usize| classes[i],
        )
    }

    /// A pick number that is not a fairness pick.
    const ORDERED: u64 = 0;
    /// A fairness pick on an even round, which starts at the bottom band.
    const FAIR: u64 = FAIRNESS_EVERY - 1;
    /// A fairness pick on the next round, which starts at the middle one.
    const FAIR_NEXT: u64 = FAIRNESS_EVERY * 2 - 1;

    #[test]
    fn the_abi_values_round_trip_and_zero_is_not_a_class() {
        for c in [Class::Latency, Class::Normal, Class::Bulk] {
            assert_eq!(Class::from_raw(c.as_raw()), Some(c));
        }
        // Zero asks a question; decoding it as a class would turn "what am I?"
        // into a silent promotion to the top band.
        assert_eq!(Class::from_raw(QUERY), None);
        assert_eq!(Class::from_raw(4), None);
        assert_eq!(Class::from_raw(u64::MAX), None);
    }

    #[test]
    fn lower_is_more_urgent() {
        assert!(Class::Latency < Class::Normal);
        assert!(Class::Normal < Class::Bulk);
        assert_eq!(Class::Latency.index(), 0);
        assert_eq!(Class::Bulk.index(), COUNT - 1);
    }

    #[test]
    fn a_latency_task_is_picked_over_a_warmer_normal_one() {
        // Slot 2 is this core's own and would win on locality alone; slot 3 is
        // latency-class and cold. Classes are consulted first, so slot 3 wins —
        // the trade this whole module exists to make.
        let classes = [Class::Normal, Class::Normal, Class::Normal, Class::Latency];
        let (pickable, sticky, class_of) = preds(&[2, 3], &[2], &classes);
        assert_eq!(
            choose(4, 0, ORDERED, &pickable, &sticky, &class_of),
            Some(3)
        );
    }

    #[test]
    fn locality_still_decides_inside_a_band() {
        // Both candidates are the same class, so the sticky pass is what is left
        // and it must still work: slot 3 is ours, slot 1 comes first in round
        // robin order.
        let classes = [Class::Normal; 4];
        let (pickable, sticky, class_of) = preds(&[1, 3], &[3], &classes);
        assert_eq!(
            choose(4, 0, ORDERED, &pickable, &sticky, &class_of),
            Some(3)
        );
    }

    #[test]
    fn a_bulk_task_waits_behind_every_normal_one() {
        let classes = [Class::Bulk, Class::Normal, Class::Bulk, Class::Normal];
        let (pickable, sticky, class_of) = preds(&[0, 1, 2, 3], &[], &classes);
        // Round-robin from 0 would give slot 1 anyway; from 1 it would give 2, and
        // the band scan must give 3 instead.
        assert_eq!(
            choose(4, 1, ORDERED, &pickable, &sticky, &class_of),
            Some(3)
        );
    }

    #[test]
    fn the_fairness_pick_serves_the_lowest_runnable_band() {
        // The starvation escape, stated as one assertion: the same ready set that
        // gives the latency task on an ordinary pick gives the bulk task on the
        // eighth.
        let classes = [Class::Normal, Class::Latency, Class::Normal, Class::Bulk];
        let (pickable, sticky, class_of) = preds(&[1, 3], &[], &classes);
        assert_eq!(
            choose(4, 0, ORDERED, &pickable, &sticky, &class_of),
            Some(1)
        );
        assert_eq!(choose(4, 0, FAIR, &pickable, &sticky, &class_of), Some(3));
    }

    #[test]
    fn a_fairness_pick_with_nothing_below_still_picks() {
        // The failure mode the reversed order exists to avoid: an escape that
        // served only the bottom band would return None here and idle a core
        // beside a runnable latency task, once in every eight picks.
        let classes = [Class::Normal, Class::Latency, Class::Normal, Class::Normal];
        let (pickable, sticky, class_of) = preds(&[1], &[], &classes);
        assert_eq!(choose(4, 0, FAIR, &pickable, &sticky, &class_of), Some(1));
    }

    #[test]
    fn the_first_pick_a_core_makes_is_not_a_fairness_pick() {
        assert!(!is_fair(0));
        assert!(is_fair(FAIRNESS_EVERY - 1));
        // Exactly one in every FAIRNESS_EVERY, which is what makes the bound on
        // the bottom band a bound and not a hope.
        let fair = (0..FAIRNESS_EVERY * 4).filter(|&p| is_fair(p)).count();
        assert_eq!(fair as u64, 4);
    }

    #[test]
    fn the_bottom_band_is_reached_within_one_period() {
        // The guarantee spelled out over a run: latency and bulk both permanently
        // runnable, and across FAIRNESS_EVERY consecutive picks the bulk slot is
        // taken at least once. With nothing in the middle band runnable, both
        // fairness rounds fall through to the bottom one.
        let classes = [Class::Latency, Class::Bulk];
        let (pickable, sticky, class_of) = preds(&[0, 1], &[], &classes);
        let taken: Vec<_> = (0..FAIRNESS_EVERY)
            .map(|p| choose_first(2, p, &pickable, &sticky, &class_of))
            .collect();
        assert_eq!(taken.iter().filter(|&&t| t == Some(1)).count(), 1);
        assert_eq!(
            taken.iter().filter(|&&t| t == Some(0)).count(),
            FAIRNESS_EVERY as usize - 1
        );
    }

    #[test]
    fn the_middle_band_is_reached_too() {
        // The regression that hung a boot. All three bands permanently runnable:
        // an escape that always started at the bottom gave the middle one nothing,
        // for ever, and the thread waiting to stop the measurement was in it.
        let classes = [Class::Latency, Class::Normal, Class::Bulk];
        let (pickable, sticky, class_of) = preds(&[0, 1, 2], &[], &classes);
        let taken: Vec<_> = (0..FAIRNESS_EVERY * 2)
            .map(|p| choose_first(3, p, &pickable, &sticky, &class_of))
            .collect();
        assert_eq!(
            taken.iter().filter(|&&t| t == Some(2)).count(),
            1,
            "the bulk slot, once"
        );
        assert_eq!(
            taken.iter().filter(|&&t| t == Some(1)).count(),
            1,
            "the normal slot, once"
        );
    }

    #[test]
    fn a_fairness_round_falls_through_to_the_other_lower_band() {
        // The middle-band round with nothing normal runnable must still reach the
        // bottom band rather than give the core back to the top one.
        let classes = [Class::Latency, Class::Normal, Class::Bulk];
        let (pickable, sticky, class_of) = preds(&[0, 2], &[], &classes);
        assert_eq!(
            choose_first(3, FAIR_NEXT, &pickable, &sticky, &class_of),
            Some(2)
        );
    }

    #[test]
    fn the_slot_we_came_from_is_still_never_returned() {
        // Inherited from `affinity::choose` and asserted here too, because the
        // band filter wraps that scan and a filter that rebuilt the range would
        // lose it silently. Only slot 2 is runnable and it is the one we are
        // leaving — in its own band, and on a fairness pick as well.
        let classes = [Class::Normal; 4];
        let (pickable, sticky, class_of) = preds(&[2], &[2], &classes);
        assert_eq!(choose(4, 2, ORDERED, &pickable, &sticky, &class_of), None);
        assert_eq!(choose(4, 2, FAIR, &pickable, &sticky, &class_of), None);
        // And an idle core, which has no `from`, may take it.
        assert_eq!(
            choose_first(4, ORDERED, &pickable, &sticky, &class_of),
            Some(2)
        );
    }

    #[test]
    fn nothing_runnable_is_none_in_every_band() {
        let classes = [Class::Normal; 4];
        let (pickable, sticky, class_of) = preds(&[], &[], &classes);
        assert_eq!(choose(4, 0, ORDERED, &pickable, &sticky, &class_of), None);
        assert_eq!(choose(4, 0, FAIR, &pickable, &sticky, &class_of), None);
        assert_eq!(choose_first(4, ORDERED, &pickable, &sticky, &class_of), None);
        assert_eq!(choose(0, 0, ORDERED, &pickable, &sticky, &class_of), None);
        assert_eq!(choose_first(0, ORDERED, &pickable, &sticky, &class_of), None);
    }

    #[test]
    fn an_idle_core_honours_classes_too() {
        // `choose_first` is a separate entry point and therefore a separate place
        // to forget the bands. Slot 0 comes first and is bulk; slot 2 is latency.
        let classes = [Class::Bulk, Class::Normal, Class::Latency];
        let (pickable, sticky, class_of) = preds(&[0, 1, 2], &[], &classes);
        assert_eq!(
            choose_first(3, ORDERED, &pickable, &sticky, &class_of),
            Some(2)
        );
        assert_eq!(choose_first(3, FAIR, &pickable, &sticky, &class_of), Some(0));
    }

    #[test]
    fn an_ordered_pick_is_never_an_inversion() {
        // The agreement the shutdown report asserts, on the path where it must
        // hold: whatever `choose` returns on an ordinary pick, the independent
        // scan finds nothing above it.
        let classes = [Class::Bulk, Class::Latency, Class::Normal, Class::Bulk];
        for ready in [&[0usize, 3][..], &[0, 2, 3][..], &[0, 1, 2, 3][..], &[3][..]] {
            let (pickable, sticky, class_of) = preds(ready, &[], &classes);
            let picked = choose_first(4, ORDERED, &pickable, &sticky, &class_of)
                .expect("something was runnable");
            assert!(
                !outranked(4, classes[picked], &pickable, &class_of),
                "ready {ready:?} picked {picked}"
            );
        }
    }

    #[test]
    fn a_fairness_pick_is_the_only_inversion() {
        let classes = [Class::Latency, Class::Bulk];
        let (pickable, sticky, class_of) = preds(&[0, 1], &[], &classes);
        let picked = choose_first(2, FAIR, &pickable, &sticky, &class_of).expect("runnable");
        assert_eq!(picked, 1);
        assert!(outranked(2, classes[picked], &pickable, &class_of));
    }

    #[test]
    fn a_preempt_that_would_demote_the_core_is_declined() {
        // The half `choose` cannot express. The incumbent is never a candidate, so
        // ordering the candidates says nothing about whether to leave at all.
        assert!(!should_switch(Class::Latency, Class::Normal, false));
        assert!(!should_switch(Class::Latency, Class::Bulk, false));
        assert!(!should_switch(Class::Normal, Class::Bulk, false));
    }

    #[test]
    fn an_equal_or_more_urgent_candidate_still_takes_the_core() {
        // Round-robin inside a band is the only fairness a band has; refusing a
        // sideways switch would pin the first task of each band to its core.
        assert!(should_switch(Class::Normal, Class::Normal, false));
        assert!(should_switch(Class::Bulk, Class::Latency, false));
        assert!(should_switch(Class::Normal, Class::Latency, false));
    }

    #[test]
    fn the_fairness_pick_overrides_the_refusal_too() {
        // Both halves of the policy need the same escape, and for the same reason:
        // a latency task that declined every preempt would own its core for ever,
        // which is a more thorough hang than merely being picked first.
        for running in [Class::Latency, Class::Normal, Class::Bulk] {
            for candidate in [Class::Latency, Class::Normal, Class::Bulk] {
                assert!(should_switch(running, candidate, true));
            }
        }
    }

    #[test]
    fn outranked_ignores_what_is_not_runnable() {
        // A blocked latency task is not something a normal pick stepped over.
        let classes = [Class::Latency, Class::Normal];
        let (pickable, _sticky, class_of) = preds(&[1], &[], &classes);
        assert!(!outranked(2, Class::Normal, &pickable, &class_of));
        assert!(!outranked(2, Class::Latency, &pickable, &class_of));
    }
}
