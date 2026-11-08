// SPDX-License-Identifier: AGPL-3.0-only
//! The subscriptions the app's one live socket carries (PLAN §10.1): every page that follows
//! something holds one under a key it chose, and a reconnect subscribes each active key again.
//!
//! Pure: no socket and no signals. [`super::socket`] keeps the callbacks and the connection state
//! of each key next to this registry.

use std::collections::BTreeMap;

use fleet_protocol::live::LiveRequest;
use uuid::Uuid;

/// What a subscription follows: a project, and one attempt's snapshot when given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The project's slug.
    pub project: String,
    /// The attempt to receive a snapshot of, if any; deltas of the whole project arrive anyway.
    pub attempt: Option<Uuid>,
}

/// The active subscriptions, by the key each was given on the socket.
#[derive(Debug, Default)]
pub struct Registry {
    issued: u64,
    active: BTreeMap<String, Subscription>,
}

impl Registry {
    /// A key this registry has not handed out before, so a page that unmounted never shares one
    /// with a page that mounted later.
    pub fn issue_key(&mut self) -> String {
        self.issued += 1;
        format!("page-{}", self.issued)
    }

    /// Makes `key` an active subscription to `subscription`.
    pub fn add(&mut self, key: String, subscription: Subscription) {
        self.active.insert(key, subscription);
    }

    /// Stops `key`. `false` when it was not active.
    pub fn remove(&mut self, key: &str) -> bool {
        self.active.remove(key).is_some()
    }

    /// The request that subscribes `key`, when it is active.
    #[must_use]
    pub fn request(&self, key: &str) -> Option<LiveRequest> {
        let subscription = self.active.get(key)?;
        Some(LiveRequest::Subscribe {
            id: Some(key.to_owned()),
            project: subscription.project.clone(),
            attempt: subscription.attempt,
        })
    }

    /// Every active subscription's request, by key. A new socket sends these first, so each page
    /// gets a fresh snapshot after a reconnect.
    #[must_use]
    pub fn requests(&self) -> Vec<LiveRequest> {
        self.active
            .keys()
            .filter_map(|key| self.request(key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(slug: &str) -> Subscription {
        Subscription {
            project: slug.to_owned(),
            attempt: None,
        }
    }

    fn keys_of(requests: &[LiveRequest]) -> Vec<String> {
        requests
            .iter()
            .map(|r| match r {
                LiveRequest::Subscribe { id, .. } => id.clone().unwrap_or_default(),
                LiveRequest::Unsubscribe { id } => id.clone(),
            })
            .collect()
    }

    #[test]
    fn a_fresh_socket_is_sent_every_active_subscription_under_its_own_key() {
        let mut r = Registry::default();
        let (a, b) = (r.issue_key(), r.issue_key());
        r.add(a.clone(), project("app"));
        r.add(
            b.clone(),
            Subscription {
                project: "app".into(),
                attempt: Some(Uuid::from_u128(7)),
            },
        );
        let sent = r.requests();
        assert_eq!(keys_of(&sent), [a.clone(), b.clone()]);
        assert_eq!(
            sent[1],
            LiveRequest::Subscribe {
                id: Some(b),
                project: "app".into(),
                attempt: Some(Uuid::from_u128(7)),
            }
        );
        assert_eq!(sent[0].clone(), r.request(&a).unwrap());
    }

    #[test]
    fn after_a_reconnect_every_active_key_is_subscribed_again_and_a_stopped_one_is_not() {
        let mut r = Registry::default();
        let (a, b, c) = (r.issue_key(), r.issue_key(), r.issue_key());
        r.add(a.clone(), project("app"));
        r.add(b.clone(), project("app"));
        // The socket drops; the page behind `b` unmounts while it is down.
        assert!(r.remove(&b));
        // The socket comes back. The subscriptions made since are added and sent with the rest.
        r.add(c.clone(), project("other"));
        let after_reconnect = r.requests();
        assert_eq!(
            keys_of(&after_reconnect),
            [a, c],
            "same keys, no stopped one"
        );
        let LiveRequest::Subscribe { project, .. } = &after_reconnect[1] else {
            panic!("a subscription, not an unsubscribe");
        };
        assert_eq!(project, "other");
    }

    #[test]
    fn a_key_that_is_not_active_has_no_request() {
        let mut r = Registry::default();
        let a = r.issue_key();
        assert_eq!(r.request(&a), None);
        r.add(a.clone(), project("app"));
        assert!(r.request(&a).is_some());
        assert!(r.remove(&a));
        assert_eq!(r.request(&a), None);
        assert!(!r.remove(&a), "a second stop finds nothing");
        assert_eq!(r.requests().len(), 0, "nothing is sent for it");
    }

    #[test]
    fn keys_are_never_handed_out_twice() {
        let mut r = Registry::default();
        let first = r.issue_key();
        r.add(first.clone(), project("app"));
        assert!(r.remove(&first));
        let second = r.issue_key();
        assert_ne!(first, second, "a stopped key is not reused");
    }

    #[test]
    fn a_request_is_the_wire_form_the_server_reads() {
        let mut r = Registry::default();
        let key = r.issue_key();
        r.add(key.clone(), project("app"));
        let wire = serde_json::to_string(&r.requests()[0]).unwrap();
        assert_eq!(
            wire,
            format!(r#"{{"op":"subscribe","id":"{key}","project":"app"}}"#)
        );
    }
}
