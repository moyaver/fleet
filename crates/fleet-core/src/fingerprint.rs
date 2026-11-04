// SPDX-License-Identifier: AGPL-3.0-only
//! Failure fingerprints (ADR-0009; PLAN §9 `failure_fingerprints`).
//!
//! A fingerprint says "these failures are the same failure". It hashes the flow's identity, the
//! failing step's kind and the failure message with its volatile parts removed
//! ([`normalize_failure_message`]). The hash carries a version prefix (`v1:`), so the algorithm
//! can evolve without silently mixing old and new groups.

use std::{borrow::Cow, collections::BTreeMap, sync::LazyLock};

use regex::Regex;
use sha2::{Digest as _, Sha256};

/// What a missing fact reads as: explicit, never guessed.
pub const UNKNOWN: &str = "unknown";

/// The failure facts a fingerprint is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureFacts<'a> {
    /// The flow's identity: its repository-relative path when known, else its name.
    pub flow: &'a str,
    /// The failing step's description as Maestro printed it, when known.
    pub step: Option<&'a str>,
    /// The failure message as reported.
    pub message: &'a str,
}

/// Placeholder for a UUID.
const UUID: &str = "<uuid>";
/// Placeholder for a date or time of day.
const TIMESTAMP: &str = "<ts>";
/// Placeholder for an IP address or `localhost`, with its port.
const ADDRESS: &str = "<addr>";
/// Placeholder for an emulator serial.
const SERIAL: &str = "<serial>";
/// Placeholder for a duration.
const DURATION: &str = "<dur>";
/// Placeholder for any number or numeric identifier, hexadecimal ones included: an id that
/// happens to be all digits must not read differently from one that is not.
const NUMBER: &str = "<n>";

/// A bare hex run is an identifier only when it holds a digit; `deadbeef` stays a word.
const BARE_HEX: &str = r"\b[0-9a-fA-F]{8,}\b";

/// Substitutions, most specific first, so a UUID is not first eaten as numbers. None of the
/// placeholders contains a digit or a `/`, which keeps [`normalize_failure_message`] idempotent.
const RULES: &[(&str, &str)] = &[
    // Paths first: they hold UUIDs, dates and `1s`-like names that must not be cut out of them.
    (UNIX_PATH, "${1}<path>"),
    (DRIVE_PATH, "${1}<path>"),
    (
        r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        UUID,
    ),
    (
        r"\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?)?",
        TIMESTAMP,
    ),
    (r"\b\d{1,2}:\d{2}:\d{2}(?:[.,]\d+)?\b", TIMESTAMP),
    (r"\bemulator-\d+\b", SERIAL),
    (r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b", ADDRESS),
    (r"\blocalhost:\d+\b", ADDRESS),
    (r"\b0x[0-9a-fA-F]+\b", NUMBER),
    (
        r"\b\d+(?:[.,]\d+)?(?:(?:ms|s|m|h)\b|\s(?:milliseconds?|secs?|seconds?|mins?|minutes?|hours?)\b)",
        DURATION,
    ),
];

/// A Windows path (`C:\x`) starts a token too. Not at a `\b`: a digit before the drive letter
/// becomes `<n>`, and its `>` would open a boundary for this rule on the second pass.
const DRIVE_PATH: &str = r#"(^|[\s"'(=\[])[A-Za-z]:\\[^\s"'()<>]+"#;
/// A path starts a token: after whitespace, a quote, a bracket or `=`.
const UNIX_PATH: &str = r#"(^|[\s"'(=\[])~?/[^\s"'()<>]+"#;
/// A leading `[shard N]`, which Maestro adds when the suite is sharded.
const SHARD_PREFIX: &str = r"^(?:\[shard \d+\]\s*)+";
/// Any other number, with its separators.
const NUMBERS: &str = r"\d+(?:[.,]\d+)*";

struct Compiled {
    shard: Option<Regex>,
    rules: Vec<(Regex, &'static str)>,
    bare_hex: Option<Regex>,
    numbers: Option<Regex>,
}

static COMPILED: LazyLock<Compiled> = LazyLock::new(|| Compiled {
    shard: Regex::new(SHARD_PREFIX).ok(),
    rules: RULES
        .iter()
        .filter_map(|(p, to)| Regex::new(p).ok().map(|r| (r, *to)))
        .collect(),
    bare_hex: Regex::new(BARE_HEX).ok(),
    numbers: Regex::new(NUMBERS).ok(),
});

/// Removes what changes between equivalent failures and keeps the words.
///
/// Only the first non-empty line counts: what follows is a stack trace, whose line numbers move
/// with every Maestro build. On that line a leading `[shard N]` goes, and so do UUIDs, dates and
/// times, emulator serials, addresses and ports, absolute paths, durations, hex identifiers and
/// every other number; each becomes a fixed placeholder (`<n>`, `<path>`, …). Quoted text keeps
/// its words (`"Login"` is what the flow asserted), only its volatile parts change. Whitespace
/// collapses to single spaces. The result is stable under a second pass.
#[must_use]
pub fn normalize_failure_message(raw: &str) -> String {
    let c = &*COMPILED;
    let line = raw
        .split(['\n', '\r'])
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let mut text = Cow::Borrowed(line);
    if let Some(re) = &c.shard {
        text = Cow::Owned(re.replace(&text, "").into_owned());
    }
    for (re, to) in &c.rules {
        text = Cow::Owned(re.replace_all(&text, *to).into_owned());
    }
    if let Some(re) = &c.bare_hex {
        text = Cow::Owned(
            re.replace_all(&text, |m: &regex::Captures<'_>| {
                let run = m.get(0).map_or("", |m| m.as_str());
                if run.chars().any(|ch| ch.is_ascii_digit()) {
                    NUMBER.to_owned()
                } else {
                    run.to_owned()
                }
            })
            .into_owned(),
        );
    }
    if let Some(re) = &c.numbers {
        text = Cow::Owned(re.replace_all(&text, NUMBER).into_owned());
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The version of the failure fingerprint algorithm: part of every fingerprint's text.
pub const FAILURE_FINGERPRINT_VERSION: u8 = 1;

/// A stable identity for "the same failure": `v1:` and 16 hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FailureFingerprint(String);

impl FailureFingerprint {
    /// The text form, for example `v1:0123456789abcdef`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FailureFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fingerprint with the normalized parts it was made from, for display and grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureSignature {
    /// The fingerprint.
    pub fingerprint: FailureFingerprint,
    /// The normalized failing step, or `unknown`.
    pub step_kind: String,
    /// The normalized failure message.
    pub message: String,
}

impl FailureFacts<'_> {
    /// Fingerprints these facts.
    #[must_use]
    pub fn signature(&self) -> FailureSignature {
        let step_kind = self
            .step
            .map(normalize_failure_message)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| UNKNOWN.to_owned());
        let message = normalize_failure_message(self.message);
        let digest = Sha256::digest(format!(
            "fleet.failure.v{FAILURE_FINGERPRINT_VERSION}\n{}\n{step_kind}\n{message}",
            self.flow
        ));
        FailureSignature {
            fingerprint: FailureFingerprint(format!(
                "v{FAILURE_FINGERPRINT_VERSION}:{}",
                hex(&digest, 8)
            )),
            step_kind,
            message,
        }
    }
}

/// The first `bytes` of `digest` as lowercase hex.
fn hex(digest: &[u8], bytes: usize) -> String {
    use std::fmt::Write as _;
    digest.iter().take(bytes).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The facts that decide whether two runs happened under comparable conditions.
///
/// A fact the agent didn't report is `None`: it reads `unknown` in the fingerprint, never a
/// guess, and an unknown value is not equal to any known one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionFacts {
    /// Device model or AVD name, as the device profile records it.
    pub device_model: Option<String>,
    /// Android API level of the device.
    pub device_api_level: Option<u32>,
    /// Maestro CLI version; only `major.minor` counts.
    pub maestro_version: Option<String>,
    /// Host operating system (`std::env::consts::OS`).
    pub os: Option<String>,
    /// Host architecture (`std::env::consts::ARCH`).
    pub arch: Option<String>,
    /// Host logical cores; bucketed.
    pub host_cores: Option<u32>,
    /// Host physical memory in bytes; bucketed.
    pub host_memory_bytes: Option<u64>,
    /// The CI provider (`github_actions`, `generic`).
    pub ci_provider: Option<String>,
}

/// The identity of a set of conditions: a SHA-256 over their canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConditionFingerprint([u8; 32]);

impl ConditionFingerprint {
    /// The raw digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Display for ConditionFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex(&self.0, 32))
    }
}

impl ConditionFacts {
    /// The dimensions as stored and hashed: bucketed, with `unknown` for what's missing.
    #[must_use]
    pub fn dims(&self) -> BTreeMap<&'static str, String> {
        let text = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map_or_else(|| UNKNOWN.to_owned(), str::to_owned)
        };
        let lower = |v: &Option<String>| text(v).to_lowercase();
        BTreeMap::from([
            ("arch", lower(&self.arch)),
            ("ci_provider", lower(&self.ci_provider)),
            (
                "device_api_level",
                self.device_api_level
                    .map_or_else(|| UNKNOWN.to_owned(), |n| n.to_string()),
            ),
            ("device_model", text(&self.device_model)),
            (
                "host_cores",
                self.host_cores
                    .filter(|n| *n > 0)
                    .map_or_else(|| UNKNOWN.to_owned(), core_class),
            ),
            (
                "host_memory",
                self.host_memory_bytes
                    .filter(|n| *n > 0)
                    .map_or_else(|| UNKNOWN.to_owned(), memory_class),
            ),
            (
                "maestro",
                self.maestro_version
                    .as_deref()
                    .and_then(major_minor)
                    .unwrap_or_else(|| UNKNOWN.to_owned()),
            ),
            ("os", lower(&self.os)),
        ])
    }

    /// The fingerprint of [`Self::dims`].
    #[must_use]
    pub fn fingerprint(&self) -> ConditionFingerprint {
        use std::fmt::Write as _;
        let mut canonical = format!("fleet.condition.v{CONDITION_FINGERPRINT_VERSION}\n");
        for (key, value) in self.dims() {
            // Length-prefixed, so no value can imitate another dimension.
            let _ = writeln!(canonical, "{key}={}:{value}", value.len());
        }
        ConditionFingerprint(Sha256::digest(canonical).into())
    }
}

/// The version of the condition fingerprint algorithm: part of the hashed text.
pub const CONDITION_FINGERPRINT_VERSION: u8 = 1;

/// The core count rounded up to a power of two, so 6 and 8 cores are one class of host.
fn core_class(cores: u32) -> String {
    match cores.next_power_of_two() {
        n if n >= 32 => "32+".to_owned(),
        n => n.to_string(),
    }
}

/// Memory in GiB rounded up to a power of two: a "16 GB" host reports about 15.6 GiB.
fn memory_class(bytes: u64) -> String {
    let gib = bytes.div_ceil(1 << 30);
    match gib.next_power_of_two() {
        n if n >= 256 => "256+GiB".to_owned(),
        n => format!("{n}GiB"),
    }
}

/// `2.12` from `2.12.1`, `v2.12` or `2.12.1-rc1`; `None` when there are no two numbers.
fn major_minor(version: &str) -> Option<String> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next()?;
    let minor: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let numeric = !major.is_empty() && major.chars().all(|c| c.is_ascii_digit());
    (numeric && !minor.is_empty()).then(|| format!("{major}.{minor}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `<failure>` text of one captured Maestro 2.12.1 scenario's JUnit report.
    macro_rules! fixture_failure {
        ($scenario:literal) => {{
            let xml = include_str!(concat!(
                "../../../fixtures/maestro/2.12.1/",
                $scenario,
                "/run/report.xml"
            ));
            let (_, rest) = xml.split_once("<failure>").unwrap();
            rest.split_once("</failure>").unwrap().0.to_owned()
        }};
    }

    #[test]
    fn the_captured_assertion_failure_normalizes_to_its_words() {
        let msg = fixture_failure!("fail");
        assert_eq!(
            normalize_failure_message(&msg),
            r#"Assertion is false: "Fleet never shows this" is visible"#
        );
    }

    #[test]
    fn a_shard_prefix_is_not_part_of_the_failure() {
        let msg = fixture_failure!("shard-all-2");
        assert_eq!(
            normalize_failure_message(&msg),
            normalize_failure_message(&fixture_failure!("fail")),
        );
    }

    #[test]
    fn the_captured_device_death_loses_its_serial_durations_and_stack() {
        let msg = fixture_failure!("device-killed");
        assert_eq!(
            normalize_failure_message(&msg),
            "maestro.android.DeviceServerDiedException: Device server died during \
             'viewHierarchy' on <serial> (<dur> since last byte, connection age <dur>): \
             StatusRuntimeException: UNAVAILABLE"
        );
    }

    fn facts<'a>(flow: &'a str, step: Option<&'a str>, message: &'a str) -> FailureFacts<'a> {
        FailureFacts {
            flow,
            step,
            message,
        }
    }

    const ASSERT_STEP: &str = r#"Assert that "Fleet never shows this" is visible"#;

    #[test]
    fn a_fingerprint_is_the_version_and_sixteen_hex_digits_of_a_pinned_hash() {
        // sha256("fleet.failure.v1\nexamples/flows/phase0/fail.yaml\nAssert that \"Fleet never
        // shows this\" is visible\nAssertion is false: ...") computed independently with
        // `sha256sum`; the first 8 bytes.
        let sig = facts(
            "examples/flows/phase0/fail.yaml",
            Some(ASSERT_STEP),
            &fixture_failure!("fail"),
        )
        .signature();
        assert_eq!(sig.fingerprint.as_str(), "v1:0ab4b3c4128f26ac");
        assert_eq!(sig.fingerprint.to_string(), sig.fingerprint.as_str());
        assert_eq!(sig.step_kind, ASSERT_STEP);
    }

    #[test]
    fn equivalent_captured_failures_share_a_fingerprint() {
        let flow = "examples/flows/phase0/fail.yaml";
        let a = facts(flow, Some(ASSERT_STEP), &fixture_failure!("fail")).signature();
        let sharded = facts(flow, Some(ASSERT_STEP), &fixture_failure!("shard-all-2")).signature();
        let retry = facts(flow, Some(ASSERT_STEP), &fixture_failure!("retry-fail")).signature();
        assert_eq!(a.fingerprint, sharded.fingerprint);
        assert_eq!(a.fingerprint, retry.fingerprint);
    }

    #[test]
    fn flow_step_and_message_each_change_the_fingerprint() {
        let base = facts("a.yaml", Some("Tap on \"Login\""), "Element not found");
        let fp = |f: FailureFacts<'_>| f.signature().fingerprint;
        let all = [
            fp(base),
            fp(FailureFacts {
                flow: "b.yaml",
                ..base
            }),
            fp(FailureFacts {
                step: Some("Tap on \"Logout\""),
                ..base
            }),
            fp(FailureFacts { step: None, ..base }),
            fp(FailureFacts {
                message: "Element found",
                ..base
            }),
        ];
        let distinct: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len());
    }

    #[test]
    fn the_device_death_is_a_different_failure_from_the_assertion() {
        let flow = "x.yaml";
        let a = facts(flow, None, &fixture_failure!("fail")).signature();
        let d = facts(flow, None, &fixture_failure!("device-killed")).signature();
        assert_ne!(a.fingerprint, d.fingerprint);
        assert!(
            d.message
                .starts_with("maestro.android.DeviceServerDiedException:")
        );
    }

    #[test]
    fn a_placeholder_never_makes_a_drive_letter_path_appear() {
        // `0` becomes `<n>`, which used to leave `A:\¡` at a word boundary for the path rule.
        let once = normalize_failure_message("0A:\\¡");
        assert_eq!(normalize_failure_message(&once), once);
    }

    mod properties {
        use proptest::prelude::*;

        use super::*;

        /// Everything that differs between two equivalent runs of the same failure.
        #[derive(Debug, Clone)]
        struct Volatile {
            serial: String,
            millis: String,
            uuid: String,
            path: String,
            at: String,
            ip: String,
            port: String,
            hex: String,
            count: String,
            order: String,
            shard: String,
        }

        fn volatile() -> impl Strategy<Value = Volatile> {
            (
                "emulator-[0-9]{4,5}",
                "[0-9]{1,6}",
                "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                "(/[a-z0-9_.-]{1,8}){2,5}",
                "20[0-9]{2}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9])T[01][0-9]:[0-5][0-9]:[0-5][0-9]Z",
                "[0-9]{1,3}(\\.[0-9]{1,3}){3}",
                "[0-9]{2,5}",
                "[0-9][0-9a-f]{7,39}",
                "[0-9]{1,6}",
                "[0-9]{3,8}",
                "[1-9]",
            )
                .prop_map(
                    |(serial, millis, uuid, path, at, ip, port, hex, count, order, shard)| {
                        Volatile {
                            serial,
                            millis,
                            uuid,
                            path,
                            at,
                            ip,
                            port,
                            hex,
                            count,
                            order,
                            shard,
                        }
                    },
                )
        }

        fn message(v: &Volatile) -> String {
            format!(
                "[shard {}] Device server died on {} ({}ms since last byte, {} seconds waiting): \
                 request {} for \"Order {} confirmed\" wrote {} (started {}, adb {}:{}, build {}), \
                 {} retries\n\tat maestro.Frame.run(Frame.kt:{})",
                v.shard,
                v.serial,
                v.millis,
                v.count,
                v.uuid,
                v.order,
                v.path,
                v.at,
                v.ip,
                v.port,
                v.hex,
                v.count,
                v.millis,
            )
        }

        proptest! {
            #[test]
            fn volatile_parts_do_not_change_the_fingerprint(a in volatile(), b in volatile()) {
                let (ma, mb) = (message(&a), message(&b));
                prop_assert_eq!(normalize_failure_message(&ma), normalize_failure_message(&mb));
                let fa = facts("f.yaml", Some("Tap on \"x\""), &ma).signature().fingerprint;
                let fb = facts("f.yaml", Some("Tap on \"x\""), &mb).signature().fingerprint;
                prop_assert_eq!(fa, fb);
            }

            #[test]
            fn a_changed_semantic_word_changes_the_fingerprint(
                w1 in "[a-z]{3,10}",
                w2 in "[a-z]{3,10}",
                v in volatile(),
            ) {
                prop_assume!(w1 != w2);
                let msg = |w: &str| format!("Assertion is false: \"{w}\" is visible on {}", v.serial);
                let (m1, m2) = (msg(&w1), msg(&w2));
                prop_assert_ne!(normalize_failure_message(&m1), normalize_failure_message(&m2));
                prop_assert_ne!(
                    facts("f.yaml", None, &m1).signature().fingerprint,
                    facts("f.yaml", None, &m2).signature().fingerprint
                );
            }

            #[test]
            fn normalizing_twice_changes_nothing(raw in any::<String>(), v in volatile()) {
                for text in [raw, message(&v)] {
                    let once = normalize_failure_message(&text);
                    prop_assert_eq!(normalize_failure_message(&once), once);
                }
            }
        }
    }

    mod conditions {
        use super::*;

        const GIB: u64 = 1 << 30;

        fn full() -> ConditionFacts {
            ConditionFacts {
                device_model: Some("Pixel_10a".into()),
                device_api_level: Some(36),
                maestro_version: Some("2.12.1".into()),
                os: Some("Linux".into()),
                arch: Some("x86_64".into()),
                host_cores: Some(6),
                host_memory_bytes: Some(15 * GIB + GIB / 2),
                ci_provider: Some("github_actions".into()),
            }
        }

        fn dims(f: &ConditionFacts) -> Vec<(&'static str, String)> {
            f.dims().into_iter().collect()
        }

        #[test]
        fn missing_facts_read_unknown_and_nothing_is_guessed() {
            assert_eq!(
                dims(&ConditionFacts::default()),
                [
                    ("arch", "unknown"),
                    ("ci_provider", "unknown"),
                    ("device_api_level", "unknown"),
                    ("device_model", "unknown"),
                    ("host_cores", "unknown"),
                    ("host_memory", "unknown"),
                    ("maestro", "unknown"),
                    ("os", "unknown"),
                ]
                .map(|(k, v)| (k, v.to_owned()))
            );
        }

        #[test]
        fn known_facts_are_bucketed_into_comparable_classes() {
            assert_eq!(
                dims(&full()),
                [
                    ("arch", "x86_64"),
                    ("ci_provider", "github_actions"),
                    ("device_api_level", "36"),
                    ("device_model", "Pixel_10a"),
                    ("host_cores", "8"),
                    ("host_memory", "16GiB"),
                    ("maestro", "2.12"),
                    ("os", "linux"),
                ]
                .map(|(k, v)| (k, v.to_owned()))
            );
        }

        #[test]
        fn the_fingerprint_is_a_pinned_sha256_of_the_canonical_dimensions() {
            // sha256 of `fleet.condition.v1\n` and one `key=<len>:<value>\n` line per dimension
            // above, in key order; computed outside this crate.
            assert_eq!(
                full().fingerprint().to_string(),
                "2dcb3466347848cc04b2f7a3252466d212eb93f94829a3c5740a981a659215b8"
            );
        }

        #[test]
        fn facts_in_the_same_class_share_a_fingerprint() {
            let patch = ConditionFacts {
                maestro_version: Some("v2.12.3".into()),
                host_cores: Some(7),
                host_memory_bytes: Some(15 * GIB + GIB / 10),
                ..full()
            };
            assert_eq!(patch.fingerprint(), full().fingerprint());
        }

        #[test]
        fn a_change_in_any_fact_class_changes_the_fingerprint() {
            let base = full();
            let variants = [
                ConditionFacts {
                    device_model: Some("Pixel_9".into()),
                    ..full()
                },
                ConditionFacts {
                    device_api_level: Some(35),
                    ..full()
                },
                ConditionFacts {
                    maestro_version: Some("2.13.0".into()),
                    ..full()
                },
                ConditionFacts {
                    os: Some("macos".into()),
                    ..full()
                },
                ConditionFacts {
                    arch: Some("aarch64".into()),
                    ..full()
                },
                ConditionFacts {
                    host_cores: Some(16),
                    ..full()
                },
                ConditionFacts {
                    host_memory_bytes: Some(32 * GIB),
                    ..full()
                },
                ConditionFacts {
                    ci_provider: Some("generic".into()),
                    ..full()
                },
            ];
            let mut seen = std::collections::HashSet::from([base.fingerprint()]);
            for v in &variants {
                assert!(seen.insert(v.fingerprint()), "{v:?} collides");
            }
        }

        #[test]
        fn unknown_is_not_equal_to_any_known_value() {
            let unknown = ConditionFacts {
                device_api_level: None,
                ..full()
            };
            assert_ne!(unknown.fingerprint(), full().fingerprint());
            let empty = ConditionFacts {
                os: Some("  ".into()),
                ..full()
            };
            let none = ConditionFacts { os: None, ..full() };
            assert_eq!(empty.fingerprint(), none.fingerprint());
        }

        #[test]
        fn an_unparsable_maestro_version_is_unknown() {
            let f = ConditionFacts {
                maestro_version: Some("snapshot".into()),
                ..full()
            };
            assert_eq!(f.dims().get("maestro").map(String::as_str), Some("unknown"));
        }
    }
}
