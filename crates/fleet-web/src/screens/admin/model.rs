// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's text: what each administration value reads as on the page. No Leptos, so it is
//! tested natively. Every string here is shown as a text node; nothing is parsed as markup.

use fleet_protocol::rest::{SessionView, admin::AdminAuditEventView};
use fleet_ui::charts::format::format_duration;
use serde_json::Value;
use time::OffsetDateTime;

/// The address of the administration page, and of the audit history.
pub const ADMIN_PATH: &str = "/admin";
/// The audit history's page.
pub const AUDIT_PATH: &str = "/admin/audit";
/// Events per page of the audit history.
pub const AUDIT_PAGE: u32 = 50;

/// Whether a signed-in person holds the administrator role on every project. Only they see the
/// Administration link. The server decides what they may read; this decides only what is offered.
#[must_use]
pub fn is_global_admin(session: &SessionView) -> bool {
    session
        .roles
        .iter()
        .any(|r| r.project.is_none() && r.role == "admin")
}

/// A project, or every project when the binding has none.
#[must_use]
pub fn scope_text(project: Option<&str>) -> String {
    project.map_or_else(|| "every project".to_owned(), str::to_owned)
}

/// A retention class's TTL: `90 days`, `36 hours`, or `kept for ever` when it has none.
#[must_use]
pub fn ttl_text(seconds: Option<i64>) -> String {
    match seconds {
        None => "kept for ever".to_owned(),
        Some(s) if s > 0 && s % 86_400 == 0 => format!("{} days", s / 86_400),
        Some(s) if s >= 3_600 => format!("{} hours", s / 3_600),
        Some(s) => format!("{s} s"),
    }
}

/// Whether a key works now, and why not if it doesn't: `active`, `revoked`, `expired`, or
/// `no expiry` for a key that never had one.
#[must_use]
pub fn key_status(
    revoked_at: Option<OffsetDateTime>,
    expires_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> &'static str {
    if revoked_at.is_some() {
        "revoked"
    } else if expires_at.is_some_and(|t| t <= now) {
        "expired"
    } else if expires_at.is_none() {
        "no expiry"
    } else {
        "active"
    }
}

/// How long ago a server-measured age was: `4.2 s ago`.
#[must_use]
pub fn seconds_ago_text(seconds: i64) -> String {
    format!("{} ago", format_duration(seconds.saturating_mul(1000)))
}

/// A lag in milliseconds: `—` when the run has no committed event yet, else `250 ms` or `3.2 s`.
#[must_use]
pub fn lag_text(ms: Option<i64>) -> String {
    ms.map_or_else(|| "—".to_owned(), format_duration)
}

/// A missing-sequence range: `3–5`, or `4` for one.
#[must_use]
pub fn seq_range(from: i64, to: i64) -> String {
    if from == to {
        from.to_string()
    } else {
        format!("{from}–{to}")
    }
}

/// A value an audit event recorded, as text: a string as it is, anything else as compact JSON.
/// `—` when there is none.
#[must_use]
pub fn detail_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "—".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Who did an audit event: a name when there is one, else the kind of actor.
#[must_use]
pub fn actor_text(kind: &str, name: Option<&str>) -> String {
    name.map_or_else(|| kind.to_owned(), str::to_owned)
}

/// The cells of one audit row, in the order of [`AUDIT_HEADERS`].
#[must_use]
pub fn audit_cells(event: &AdminAuditEventView, ctx: &crate::screens::common::Ctx) -> Vec<String> {
    vec![
        ctx.time(crate::clock::unix_ms(event.at)),
        actor_text(&event.actor_kind, event.actor.as_deref()),
        event.action.clone(),
        event.project.clone().unwrap_or_else(|| "—".to_owned()),
        event
            .target_kind
            .clone()
            .map_or_else(|| "—".to_owned(), |k| k.replace('_', " ")),
        detail_text(event.before.as_ref()),
        detail_text(event.after.as_ref()),
        if event
            .details
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            "—".to_owned()
        } else {
            event.details.to_string()
        },
    ]
}

/// The audit history's column titles.
pub const AUDIT_HEADERS: [&str; 8] = [
    "When", "Who", "What", "Project", "Target", "Before", "After", "Recorded",
];

#[cfg(test)]
mod tests {
    use fleet_protocol::rest::{SessionRole, SessionUser};
    use serde_json::json;
    use time::Duration;
    use uuid::Uuid;

    use super::*;

    fn session(roles: &[(Option<&str>, &str)]) -> SessionView {
        SessionView {
            user: SessionUser {
                id: Uuid::nil(),
                email: None,
                name: Some("Ada".into()),
            },
            roles: roles
                .iter()
                .map(|(project, role)| SessionRole {
                    project: project.map(str::to_owned),
                    role: (*role).to_owned(),
                })
                .collect(),
            csrf_token: "t".into(),
        }
    }

    #[test]
    fn only_an_admin_of_every_project_gets_the_administration_link() {
        assert!(is_global_admin(&session(&[(None, "admin")])));
        assert!(
            !is_global_admin(&session(&[(Some("app"), "admin")])),
            "a project admin is not"
        );
        assert!(!is_global_admin(&session(&[(None, "maintainer")])));
        assert!(!is_global_admin(&session(&[
            (None, "viewer"),
            (Some("app"), "viewer")
        ])));
    }

    #[test]
    fn a_ttl_reads_in_days_or_hours_and_none_is_kept_for_ever() {
        assert_eq!(ttl_text(None), "kept for ever");
        assert_eq!(ttl_text(Some(90 * 86_400)), "90 days");
        assert_eq!(ttl_text(Some(36 * 3_600)), "36 hours");
        assert_eq!(ttl_text(Some(90)), "90 s");
    }

    #[test]
    fn a_key_is_active_until_it_is_revoked_or_expires() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(10);
        let later = now + Duration::days(1);
        let earlier = now - Duration::days(1);
        assert_eq!(key_status(None, Some(later), now), "active");
        assert_eq!(key_status(None, Some(earlier), now), "expired");
        assert_eq!(key_status(Some(earlier), Some(later), now), "revoked");
        assert_eq!(key_status(None, None, now), "no expiry");
    }

    #[test]
    fn a_missing_value_reads_as_a_dash_and_a_recorded_one_as_it_is() {
        assert_eq!(lag_text(None), "—");
        assert_eq!(lag_text(Some(250)), "250 ms");
        assert_eq!(lag_text(Some(3_200)), "3.2 s");
        assert_eq!(detail_text(None), "—");
        assert_eq!(detail_text(Some(&json!(null))), "—");
        assert_eq!(detail_text(Some(&json!("known"))), "known");
        assert_eq!(detail_text(Some(&json!({"a": 1}))), r#"{"a":1}"#);
        assert_eq!(seq_range(4, 4), "4");
        assert_eq!(seq_range(3, 5), "3–5");
        assert_eq!(scope_text(None), "every project");
        assert_eq!(actor_text("system", None), "system");
    }

    #[test]
    fn hostile_text_stays_text_in_every_cell_it_reaches() {
        let hostile = "</td><img src=x onerror=alert(1)>";
        let event = AdminAuditEventView {
            id: Uuid::nil(),
            at: OffsetDateTime::UNIX_EPOCH,
            actor_kind: "user".into(),
            actor_id: None,
            actor: Some(hostile.into()),
            action: "failure.triage".into(),
            project: Some(hostile.into()),
            target_kind: None,
            target_id: None,
            before: Some(json!(hostile)),
            after: None,
            details: json!({"issue_url": hostile}),
        };
        let ctx = crate::screens::common::Ctx {
            project: "app".into(),
            now_ms: 0,
            utc_offset_minutes: 0,
        };
        let cells = audit_cells(&event, &ctx);
        assert_eq!(cells.len(), AUDIT_HEADERS.len());
        assert_eq!(cells[1], hostile, "the actor is shown as the name it has");
        assert_eq!(cells[5], hostile);
        assert!(
            cells[7].contains(hostile),
            "the details keep the text as stored"
        );
    }
}
