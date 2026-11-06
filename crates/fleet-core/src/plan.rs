// SPDX-License-Identifier: AGPL-3.0-only
//! Shard balance (ADR-0013, issue #81): what an equal-count split of a plan would have produced,
//! so a planned pipeline can show how much the duration-aware plan saved. Pure arithmetic over
//! the estimates the planner used.

/// The largest shard's total, in the same unit as `units`, when `units` are dealt into `shards`
/// by count alone.
///
/// Each unit is a scheduling unit (an `executionOrder` sequence, or one flow) given as its
/// flows' durations; a unit's flows always land in one shard, in order. Units are taken in the
/// order given, and each goes to the shard with the fewest flows so far (the lowest index on a
/// tie). The durations are ignored when choosing a shard; they only sum into the result.
///
/// Capability requirements are ignored: this is a baseline, not a plan. Returns 0 for no units
/// or no shards.
#[must_use]
pub fn equal_count_max(units: &[Vec<u64>], shards: u32) -> u64 {
    let Ok(shards) = usize::try_from(shards) else {
        return 0;
    };
    // (flows, total duration) per shard.
    let mut slots = vec![(0_usize, 0_u64); shards];
    for unit in units {
        let Some(target) = slots.iter_mut().min_by_key(|slot| slot.0) else {
            return 0;
        };
        target.0 = target.0.saturating_add(unit.len());
        target.1 = unit
            .iter()
            .fold(target.1, |total, duration| total.saturating_add(*duration));
    }
    slots.iter().map(|slot| slot.1).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::equal_count_max;

    #[test]
    fn an_executionorder_sequence_stays_in_one_shard() {
        // Units in planner order: a three-flow sequence, then four single flows, two shards.
        // The sequence fills shard 0 (3 flows, 15 s); the singles then go to shard 1 until it
        // has three flows too, and the fourth ties and goes to shard 0 (lowest index).
        let units = vec![vec![5, 5, 5], vec![1], vec![1], vec![1], vec![1]];
        assert_eq!(equal_count_max(&units, 2), 16);
        // The same flows as independent units balance better: shard 0 gets 5 + 5 + 1 + 1 = 12.
        let split = vec![
            vec![5],
            vec![5],
            vec![5],
            vec![1],
            vec![1],
            vec![1],
            vec![1],
        ];
        assert_eq!(equal_count_max(&split, 2), 12);
    }

    #[test]
    fn units_are_dealt_by_count_not_by_duration() {
        // Count alone puts the two long flows in different shards, the same as a naive split:
        // shard 0 gets 10 + 1, shard 1 gets 1 + 10.
        let units = vec![vec![10], vec![1], vec![1], vec![10]];
        assert_eq!(equal_count_max(&units, 2), 11);
    }

    #[test]
    fn one_shard_holds_everything_and_nothing_gives_zero() {
        assert_eq!(equal_count_max(&[vec![3, 4], vec![5]], 1), 12);
        assert_eq!(equal_count_max(&[], 3), 0);
        assert_eq!(equal_count_max(&[vec![7]], 0), 0);
    }
}
