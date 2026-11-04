// SPDX-License-Identifier: AGPL-3.0-only
//! Publisher trust policies (ADR-0008, research 06 §2.5–§2.6): may a verified CI OIDC identity
//! publish into a project, and how much is it trusted?
//!
//! Matching uses immutable numeric claims (`repository_id`, `repository_owner_id`), never
//! `sub`. `pull_request_target` and `dynamic` (Dependabot) are always refused; a fork PR has no
//! token at all and runs offline.

/// How much a publisher is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// A push, schedule or dispatch on a trusted ref: counts toward history and baselines.
    Trusted,
    /// A pull request, or an untrusted ref: shown, but kept out of baselines.
    Pr,
}

/// What to do with pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrPolicy {
    /// Accept them at `Pr` trust (the default).
    Restricted,
    /// Accept them at `Pr` trust; artifacts follow the full policy (decided at upload, M5).
    Full,
    /// Refuse them.
    Deny,
}

/// One project's rule for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Immutable repository ID.
    pub repository_id: String,
    /// Immutable owner ID.
    pub repository_owner_id: String,
    /// Refs (with `*` wildcards) whose pushes are trusted.
    pub trusted_refs: Vec<String>,
    /// Events accepted at all; empty means [`DEFAULT_EVENTS`].
    pub allowed_events: Vec<String>,
    /// Pull requests.
    pub pr_policy: PrPolicy,
    /// When set, only tokens minted inside this reusable workflow.
    pub job_workflow_ref: Option<String>,
    /// When set, only jobs in this deployment environment.
    pub environment: Option<String>,
}

/// The verified claims a decision needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    /// `repository_id`.
    pub repository_id: Option<String>,
    /// `repository_owner_id`.
    pub repository_owner_id: Option<String>,
    /// `ref`.
    pub git_ref: Option<String>,
    /// `event_name`.
    pub event_name: Option<String>,
    /// `job_workflow_ref`.
    pub job_workflow_ref: Option<String>,
    /// `environment`.
    pub environment: Option<String>,
}

/// Events accepted when a policy lists none.
pub const DEFAULT_EVENTS: &[&str] = &[
    "push",
    "schedule",
    "workflow_dispatch",
    "merge_group",
    "pull_request",
];

/// Events never accepted: they run untrusted code with the base repository's privileges.
pub const REFUSED_EVENTS: &[&str] = &["pull_request_target", "dynamic"];

/// The outcome of [`evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Accepted at this trust.
    Allow(Trust),
    /// Refused, and why.
    Deny(&'static str),
}

/// Decides whether `claims` may publish under `policy`.
#[must_use]
pub fn evaluate(policy: &Policy, claims: &Claims) -> Decision {
    if claims.repository_id.as_deref() != Some(policy.repository_id.as_str())
        || claims.repository_owner_id.as_deref() != Some(policy.repository_owner_id.as_str())
    {
        return Decision::Deny("repository does not match");
    }
    if let Some(w) = &policy.job_workflow_ref
        && claims.job_workflow_ref.as_ref() != Some(w)
    {
        return Decision::Deny("not the required reusable workflow");
    }
    if let Some(env) = &policy.environment
        && claims.environment.as_ref() != Some(env)
    {
        return Decision::Deny("not the required environment");
    }
    let Some(event) = claims.event_name.as_deref() else {
        return Decision::Deny("no event_name claim");
    };
    if REFUSED_EVENTS.contains(&event) {
        return Decision::Deny("event refused for untrusted code");
    }
    let allowed = if policy.allowed_events.is_empty() {
        DEFAULT_EVENTS.contains(&event)
    } else {
        policy.allowed_events.iter().any(|e| e == event)
    };
    if !allowed {
        return Decision::Deny("event not allowed");
    }
    if event == "pull_request" {
        return match policy.pr_policy {
            PrPolicy::Deny => Decision::Deny("pull requests refused"),
            PrPolicy::Restricted | PrPolicy::Full => Decision::Allow(Trust::Pr),
        };
    }
    let trusted = claims
        .git_ref
        .as_deref()
        .is_some_and(|r| policy.trusted_refs.iter().any(|g| glob(g, r)));
    Decision::Allow(if trusted { Trust::Trusted } else { Trust::Pr })
}

/// `*` matches any run of characters (including `/`); everything else is literal.
#[must_use]
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return text.is_empty();
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for m in middle {
        match rest.find(m) {
            Some(i) => rest = rest.get(i + m.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn policy() -> Policy {
        Policy {
            repository_id: "123".into(),
            repository_owner_id: "9".into(),
            trusted_refs: vec!["refs/heads/main".into(), "refs/tags/v*".into()],
            allowed_events: vec![],
            pr_policy: PrPolicy::Restricted,
            job_workflow_ref: None,
            environment: None,
        }
    }

    fn claims(event: &str, git_ref: &str) -> Claims {
        Claims {
            repository_id: Some("123".into()),
            repository_owner_id: Some("9".into()),
            git_ref: Some(git_ref.into()),
            event_name: Some(event.into()),
            ..Claims::default()
        }
    }

    #[test]
    fn trust_follows_event_and_ref() {
        let p = policy();
        assert_eq!(
            evaluate(&p, &claims("push", "refs/heads/main")),
            Decision::Allow(Trust::Trusted)
        );
        assert_eq!(
            evaluate(&p, &claims("push", "refs/tags/v1.2")),
            Decision::Allow(Trust::Trusted)
        );
        assert_eq!(
            evaluate(&p, &claims("push", "refs/heads/feature")),
            Decision::Allow(Trust::Pr)
        );
        assert_eq!(
            evaluate(&p, &claims("pull_request", "refs/heads/main")),
            Decision::Allow(Trust::Pr)
        );
        assert!(matches!(
            evaluate(&p, &claims("pull_request_target", "refs/heads/main")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            evaluate(&p, &claims("dynamic", "refs/heads/main")),
            Decision::Deny(_)
        ));
        assert!(matches!(
            evaluate(&p, &claims("issue_comment", "refs/heads/main")),
            Decision::Deny(_)
        ));
        let deny_prs = Policy {
            pr_policy: PrPolicy::Deny,
            ..p.clone()
        };
        assert!(matches!(
            evaluate(&deny_prs, &claims("pull_request", "x")),
            Decision::Deny(_)
        ));
        let mut other = claims("push", "refs/heads/main");
        other.repository_id = Some("124".into());
        assert!(
            matches!(evaluate(&p, &other), Decision::Deny(_)),
            "numeric IDs, not names"
        );
        let pinned = Policy {
            job_workflow_ref: Some("o/ci/.github/workflows/m.yml@refs/heads/main".into()),
            ..p
        };
        assert!(matches!(
            evaluate(&pinned, &claims("push", "refs/heads/main")),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn globs() {
        assert!(glob("refs/heads/*", "refs/heads/a/b"));
        assert!(glob("refs/*/v*", "refs/tags/v1"));
        assert!(!glob("refs/heads/main", "refs/heads/main2"));
        assert!(glob("*", ""));
        assert!(!glob("a*b", "ba"));
    }

    proptest! {
        /// Refused events and foreign repositories never pass, whatever else is configured.
        #[test]
        fn refusals_hold(event in prop::sample::select(vec!["pull_request_target", "dynamic"]),
                         git_ref in ".*", extra in prop::collection::vec("[a-z_]{1,12}", 0..4)) {
            let mut p = policy();
            p.allowed_events = extra.clone();
            p.allowed_events.push(event.to_owned());
            p.trusted_refs.push("*".into());
            prop_assert!(matches!(evaluate(&p, &claims(event, &git_ref)), Decision::Deny(_)));
            let mut c = claims("push", &git_ref);
            c.repository_owner_id = Some("10".into());
            prop_assert!(matches!(evaluate(&p, &c), Decision::Deny(_)));
        }
    }
}
