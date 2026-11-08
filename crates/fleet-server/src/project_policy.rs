// SPDX-License-Identifier: AGPL-3.0-only
//! What applies to a project (issue #139): the configured value of each limit and metric setting,
//! unless the project overrides it on the dashboard. The configuration sets the server-wide value; an
//! override can only tighten a server-wide size cap, which the store does not check, so this module
//! takes the lower of the two wherever a cap is read. Everything the upload path, the welcome and the
//! Administration page show comes from here, so they cannot disagree.

use fleet_protocol::{
    artifact::ArtifactClass,
    rest::admin::{AdminArtifactClassView, AdminProjectLimitsView, AdminProjectPolicyView},
};
use fleet_store::admin::policy::{ARTIFACT_CLASSES, ProjectOverrides};

use crate::config::Config;

/// The sampling interval while the host is calm, when neither the project nor the configuration
/// sets one: the agent's own default.
pub(crate) const DEFAULT_SAMPLE_MS: u64 = 5_000;
/// The sampling interval under host pressure, when neither sets one.
pub(crate) const DEFAULT_PRESSURE_SAMPLE_MS: u64 = 15_000;

/// What one artifact class of a project accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClassRule {
    /// Whether the project accepts the class at all.
    pub(crate) accepted: bool,
    /// The largest file accepted, in bytes: never above the configured cap.
    pub(crate) max_bytes: u64,
}

/// A project's rule for `class`: its override where it has one, the configured cap otherwise, and
/// never more than the cap.
pub(crate) fn class_rule(
    config: &Config,
    overrides: &ProjectOverrides,
    class: ArtifactClass,
) -> ClassRule {
    let cap = config.artifacts.max_bytes.of(class);
    let own = overrides.class(class.as_str());
    ClassRule {
        accepted: own.is_none_or(|c| c.accepted),
        max_bytes: own
            .and_then(|c| c.max_bytes)
            .and_then(|m| u64::try_from(m).ok())
            .map_or(cap, |m| m.min(cap)),
    }
}

/// A project's daily quota: bytes and files declared in 24 hours, never above the configured
/// values.
pub(crate) fn daily_quota(config: &Config, overrides: &ProjectOverrides) -> (u64, u32) {
    let a = &config.artifacts;
    let bytes = overrides
        .bytes_per_day
        .and_then(|v| u64::try_from(v).ok())
        .map_or(a.max_project_bytes_per_day, |v| {
            v.min(a.max_project_bytes_per_day)
        });
    let files = overrides
        .files_per_day
        .and_then(|v| u32::try_from(v).ok())
        .map_or(a.max_project_files_per_day, |v| {
            v.min(a.max_project_files_per_day)
        });
    (bytes, files)
}

/// The sampling intervals a project's agents are told in the welcome: the project's, else the
/// configured ones. `None` leaves the agent's default in place.
pub(crate) fn sampling(
    config: &Config,
    overrides: &ProjectOverrides,
) -> (Option<u64>, Option<u64>) {
    (
        overrides
            .sample_interval_ms
            .and_then(|v| u64::try_from(v).ok())
            .or(config.ingest.sample_interval_ms),
        overrides
            .pressure_sample_interval_ms
            .and_then(|v| u64::try_from(v).ok())
            .or(config.ingest.pressure_sample_interval_ms),
    )
}

/// The policy as the Administration page shows it, for every project and every artifact class.
pub(crate) fn view(config: &Config, projects: &[ProjectOverrides]) -> AdminProjectPolicyView {
    let mut classes = Vec::new();
    let mut limits = Vec::new();
    for o in projects {
        for name in ARTIFACT_CLASSES {
            let Some(class) = ArtifactClass::parse(name) else {
                continue;
            };
            let rule = class_rule(config, o, class);
            classes.push(AdminArtifactClassView {
                project: o.slug.clone(),
                class: name.to_owned(),
                accepted: rule.accepted,
                max_bytes: i64::try_from(rule.max_bytes).unwrap_or(i64::MAX),
                configured_max_bytes: i64::try_from(config.artifacts.max_bytes.of(class))
                    .unwrap_or(i64::MAX),
                overridden: o.class(name).is_some(),
            });
        }
        let (bytes, files) = daily_quota(config, o);
        let (sample, pressure) = sampling(config, o);
        limits.push(AdminProjectLimitsView {
            project: o.slug.clone(),
            bytes_per_day: i64::try_from(bytes).unwrap_or(i64::MAX),
            bytes_per_day_overridden: o.bytes_per_day.is_some(),
            files_per_day: i64::from(files),
            files_per_day_overridden: o.files_per_day.is_some(),
            sample_interval_ms: i64::try_from(sample.unwrap_or(DEFAULT_SAMPLE_MS))
                .unwrap_or(i64::MAX),
            sample_interval_overridden: o.sample_interval_ms.is_some(),
            pressure_sample_interval_ms: i64::try_from(
                pressure.unwrap_or(DEFAULT_PRESSURE_SAMPLE_MS),
            )
            .unwrap_or(i64::MAX),
            pressure_sample_interval_overridden: o.pressure_sample_interval_ms.is_some(),
        });
    }
    AdminProjectPolicyView {
        classes,
        projects: limits,
    }
}

#[cfg(test)]
mod tests {
    use fleet_store::admin::policy::ClassOverride;
    use uuid::Uuid;

    use super::*;

    fn overrides(classes: Vec<ClassOverride>) -> ProjectOverrides {
        ProjectOverrides {
            project: Uuid::nil(),
            slug: "app".into(),
            classes,
            bytes_per_day: None,
            files_per_day: None,
            sample_interval_ms: None,
            pressure_sample_interval_ms: None,
        }
    }

    #[test]
    fn a_class_without_an_override_is_accepted_at_the_configured_cap() {
        let config = Config::default();
        let rule = class_rule(&config, &overrides(vec![]), ArtifactClass::Junit);
        assert_eq!(
            rule,
            ClassRule {
                accepted: true,
                max_bytes: config.artifacts.max_bytes.junit
            }
        );
    }

    #[test]
    fn an_override_can_refuse_a_class_or_lower_its_cap_but_never_raise_it() {
        let config = Config::default();
        let cap = config.artifacts.max_bytes.junit;
        let off = overrides(vec![ClassOverride {
            class: "junit".into(),
            accepted: false,
            max_bytes: None,
        }]);
        assert!(!class_rule(&config, &off, ArtifactClass::Junit).accepted);

        let small = overrides(vec![ClassOverride {
            class: "junit".into(),
            accepted: true,
            max_bytes: Some(1024),
        }]);
        assert_eq!(
            class_rule(&config, &small, ArtifactClass::Junit).max_bytes,
            1024
        );

        let raised = overrides(vec![ClassOverride {
            class: "junit".into(),
            accepted: true,
            max_bytes: Some(i64::MAX),
        }]);
        assert_eq!(
            class_rule(&config, &raised, ArtifactClass::Junit).max_bytes,
            cap
        );
    }

    #[test]
    fn a_daily_quota_override_is_capped_at_the_configured_value() {
        let config = Config::default();
        let mut o = overrides(vec![]);
        o.bytes_per_day = Some(1_000);
        o.files_per_day = Some(i32::MAX);
        let (bytes, files) = daily_quota(&config, &o);
        assert_eq!(bytes, 1_000);
        assert_eq!(files, config.artifacts.max_project_files_per_day);
    }

    #[test]
    fn sampling_is_the_projects_then_the_configured_then_the_agents_default() {
        let mut config = Config::default();
        let mut o = overrides(vec![]);
        assert_eq!(
            sampling(&config, &o),
            (None, None),
            "the agent keeps 5 s and 15 s"
        );
        config.ingest.sample_interval_ms = Some(2_000);
        assert_eq!(sampling(&config, &o), (Some(2_000), None));
        o.sample_interval_ms = Some(3_000);
        o.pressure_sample_interval_ms = Some(30_000);
        assert_eq!(sampling(&config, &o), (Some(3_000), Some(30_000)));
    }

    #[test]
    fn the_view_names_each_override_and_shows_the_defaults_otherwise() {
        let config = Config::default();
        let mut o = overrides(vec![ClassOverride {
            class: "commands".into(),
            accepted: false,
            max_bytes: None,
        }]);
        o.sample_interval_ms = Some(1_000);
        let view = view(&config, &[o]);
        assert_eq!(view.classes.len(), ARTIFACT_CLASSES.len());
        let commands = view.classes.iter().find(|c| c.class == "commands").unwrap();
        assert!(!commands.accepted && commands.overridden);
        let junit = view.classes.iter().find(|c| c.class == "junit").unwrap();
        assert!(junit.accepted && !junit.overridden);
        let limits = &view.projects[0];
        assert_eq!(limits.sample_interval_ms, 1_000);
        assert!(limits.sample_interval_overridden);
        assert_eq!(limits.pressure_sample_interval_ms, 15_000);
        assert!(!limits.pressure_sample_interval_overridden);
    }
}
