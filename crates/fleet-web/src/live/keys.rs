// SPDX-License-Identifier: AGPL-3.0-only
//! The keys of a keyed list of entities (PLAN §5.4 pitfalls).
//!
//! Rows are keyed `(epoch, id)`. While the epoch holds, a row keeps living across updates and
//! only its own signal changes. When the live store re-snapshots, the epoch changes, so every key
//! changes and every row is rebuilt: a keyed list that diffed across the gap kept stale rows
//! with no error.

use uuid::Uuid;

/// A row's identity: the snapshot generation and the entity id.
pub type Key = (u64, Uuid);

/// What changed in the set of keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A new snapshot generation (or the first one): drop every per-entity signal and start over.
    Rebuilt,
    /// Same generation; entities appeared or disappeared.
    Membership,
}

/// The keys currently shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys {
    epoch: u64,
    ids: Vec<Uuid>,
}

impl Keys {
    /// Moves to `epoch` holding `ids`. `None` when nothing changed, so no signal has to be set.
    pub fn update(&mut self, epoch: u64, mut ids: Vec<Uuid>) -> Option<Change> {
        ids.sort_unstable();
        let change = if epoch != self.epoch {
            Change::Rebuilt
        } else if ids != self.ids {
            Change::Membership
        } else {
            return None;
        };
        self.epoch = epoch;
        self.ids = ids;
        Some(change)
    }

    /// The keys, ordered by id.
    #[must_use]
    pub fn keys(&self) -> Vec<Key> {
        self.ids.iter().map(|id| (self.epoch, *id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn the_first_snapshot_builds_the_keys() {
        let mut keys = Keys::default();
        assert_eq!(keys.update(1, vec![id(2), id(1)]), Some(Change::Rebuilt));
        assert_eq!(keys.keys(), [(1, id(1)), (1, id(2))]);
    }

    #[test]
    fn unchanged_membership_in_the_same_epoch_changes_nothing() {
        let mut keys = Keys::default();
        let _ = keys.update(1, vec![id(1), id(2)]);
        assert_eq!(keys.update(1, vec![id(2), id(1)]), None);
    }

    #[test]
    fn entities_coming_and_going_keep_the_epoch_and_the_other_keys() {
        let mut keys = Keys::default();
        let _ = keys.update(1, vec![id(1), id(2)]);
        assert_eq!(
            keys.update(1, vec![id(1), id(2), id(3)]),
            Some(Change::Membership)
        );
        assert_eq!(keys.keys(), [(1, id(1)), (1, id(2)), (1, id(3))]);
        assert_eq!(keys.update(1, vec![id(2), id(3)]), Some(Change::Membership));
        assert_eq!(keys.keys(), [(1, id(2)), (1, id(3))]);
    }

    #[test]
    fn a_re_snapshot_with_the_same_entities_still_changes_every_key() {
        let mut keys = Keys::default();
        let _ = keys.update(1, vec![id(1), id(2)]);
        assert_eq!(keys.update(2, vec![id(1), id(2)]), Some(Change::Rebuilt));
        assert_eq!(keys.keys(), [(2, id(1)), (2, id(2))]);
    }
}
