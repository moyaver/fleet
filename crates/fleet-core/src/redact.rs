// SPDX-License-Identifier: AGPL-3.0-only
//! Secret redaction (invariant I6).
//!
//! Research 08 found planted secret values in clear in `commands.json`, the runner's logs and the
//! argv. Everything that leaves the runner goes through a [`Redactor`] built from the secret
//! values the runner adapter names (`fleet-maestro::redact` for Maestro), and from well-known
//! token shapes. This module knows no runner's flags or variables.

use std::{borrow::Cow, fmt::Write as _};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use regex::Regex;

/// What a redacted value becomes.
pub const MARKER: &str = "[REDACTED]";

/// Values shorter than this are not treated as secrets: redacting `1` or `true` everywhere would
/// destroy the logs and protect nothing.
const MIN_SECRET_LEN: usize = 4;

/// Name fragments that make an environment variable a secret.
const SECRET_NAME_PARTS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASS",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
    "SESSION",
    "COOKIE",
];

/// Token shapes that are secrets wherever they appear.
const PATTERNS: &[&str] = &[
    r"gh[pousr]_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"mf_live_[A-Za-z0-9]{16,}",
    r"AKIA[0-9A-Z]{16}",
    r"xox[abposr]-[A-Za-z0-9-]{10,}",
    r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}",
    r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
];

/// Replaces known secret values and token shapes with [`MARKER`].
#[derive(Debug, Clone)]
pub struct Redactor {
    /// Longest first, so a secret that contains another is replaced whole.
    values: Vec<String>,
    patterns: Vec<Regex>,
}

impl Redactor {
    /// Builds a redactor from secret values. Short values are ignored (see `MIN_SECRET_LEN`);
    /// each value is also matched in its JSON, percent-encoded and base64 forms.
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut all: Vec<String> = Vec::new();
        for v in values {
            if v.chars().count() < MIN_SECRET_LEN {
                continue;
            }
            all.extend([
                json_escaped(&v),
                percent_encoded(&v, true),
                percent_encoded(&v, false),
                STANDARD.encode(v.as_bytes()),
                STANDARD_NO_PAD.encode(v.as_bytes()),
                URL_SAFE.encode(v.as_bytes()),
                URL_SAFE_NO_PAD.encode(v.as_bytes()),
                v,
            ]);
        }
        all.sort_by_key(|v| std::cmp::Reverse(v.len()));
        all.dedup();
        let patterns = PATTERNS.iter().filter_map(|p| Regex::new(p).ok()).collect();
        Self {
            values: all,
            patterns,
        }
    }

    /// The text with every secret replaced.
    #[must_use]
    pub fn redact<'t>(&self, text: &'t str) -> Cow<'t, str> {
        let mut out = Cow::Borrowed(text);
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = Cow::Owned(out.replace(v.as_str(), MARKER));
            }
        }
        for p in &self.patterns {
            if p.is_match(&out) {
                out = Cow::Owned(p.replace_all(&out, MARKER).into_owned());
            }
        }
        out
    }
}

/// Whether an environment variable's name marks it as a secret (its upper-cased name contains a
/// secret word such as `TOKEN` or `PASSWORD`).
#[must_use]
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAME_PARTS.iter().any(|p| upper.contains(p))
}

/// Where the reader of a command line is, for [`redact_argv`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A fresh word: a flag, a subcommand or a positional.
    Word,
    /// The value of a flag whose value is known to be safe: kept as it is.
    Keep,
    /// The value of `-e`/`--env`: rewritten to `KEY=[REDACTED]`.
    Env,
    /// The value of a flag nobody has classified: redacted.
    Unknown,
}

/// Records a command line without its secrets (I6, research 06 T5).
///
/// `safe_value_flag` names the flags whose value is known not to be a secret; `boolean_flag`
/// names the flags that take no value, so the word after them is not theirs; `env_flag` names
/// the flags that take a `KEY=VALUE` pair. The value of any other flag is replaced with
/// [`MARKER`]. The value of an `env_flag` is replaced with `KEY=[REDACTED]`, whatever it holds.
/// Words that are not flags are kept, and so is everything after `--`. The program name is kept.
#[must_use]
pub fn redact_argv(
    argv: &[String],
    safe_value_flag: impl Fn(&str) -> bool,
    boolean_flag: impl Fn(&str) -> bool,
    env_flag: impl Fn(&str) -> bool,
) -> Vec<String> {
    let flag_like = |word: &str| {
        let name = word.split_once('=').map_or(word, |(n, _)| n);
        name.starts_with("--") || env_flag(name) || boolean_flag(name) || safe_value_flag(name)
    };
    let mut out = Vec::with_capacity(argv.len());
    let mut words = argv.iter();
    out.extend(words.next().cloned());
    let mut expect = Expect::Word;
    let mut literal = false;
    for word in words {
        match expect {
            Expect::Keep => {
                out.push(word.clone());
                expect = Expect::Word;
                continue;
            }
            Expect::Env => {
                out.push(env_pair(word));
                expect = Expect::Word;
                continue;
            }
            Expect::Unknown if !flag_like(word) => {
                out.push(MARKER.to_owned());
                expect = Expect::Word;
                continue;
            }
            Expect::Unknown | Expect::Word => expect = Expect::Word,
        }
        if literal || !word.starts_with('-') || word == "-" {
            out.push(word.clone());
            continue;
        }
        if word == "--" {
            literal = true;
            out.push(word.clone());
            continue;
        }
        let (name, inline) = word
            .split_once('=')
            .map_or((word.as_str(), None), |(n, v)| (n, Some(v)));
        if env_flag(name) {
            if let Some(kv) = inline {
                out.push(format!("{name}={}", env_pair(kv)));
            } else {
                out.push(word.clone());
                expect = Expect::Env;
            }
        } else if boolean_flag(name) && inline.is_none() {
            out.push(word.clone());
        } else if safe_value_flag(name) {
            out.push(word.clone());
            expect = if inline.is_none() {
                Expect::Keep
            } else {
                Expect::Word
            };
        } else if inline.is_some() {
            out.push(format!("{name}={MARKER}"));
        } else {
            out.push(word.clone());
            expect = Expect::Unknown;
        }
    }
    out
}

/// `KEY=value` as `KEY=[REDACTED]`; a word with no `=` is wholly redacted.
fn env_pair(kv: &str) -> String {
    match kv.split_once('=') {
        Some((key, _)) => format!("{key}={MARKER}"),
        None => MARKER.to_owned(),
    }
}

fn percent_encoded(value: &str, uppercase: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else if uppercase {
            let _ = write!(out, "%{byte:02X}");
        } else {
            let _ = write!(out, "%{byte:02x}");
        }
    }
    out
}

fn json_escaped(v: &str) -> String {
    let quoted = serde_json_escape(v);
    quoted.trim_matches('"').to_owned()
}

/// Minimal JSON string escaping, matching what Maestro's JSON writer emits.
fn serde_json_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn secret_words_in_names_mark_secrets() {
        assert!(is_secret_name("api_token"));
        assert!(is_secret_name("PIN_PASSWORD"));
        assert!(!is_secret_name("HOME"));
    }

    #[test]
    fn redacts_json_escaped_values_and_token_shapes() {
        let r = Redactor::new(["pa\"ss\\word".to_owned()]);
        assert_eq!(
            r.redact(r#"{"text":"pa\"ss\\word"}"#),
            r#"{"text":"[REDACTED]"}"#
        );
        let r = Redactor::new([]);
        assert_eq!(
            r.redact("token ghp_abcdefghijklmnopqrstuvwxyz0123"),
            "token [REDACTED]"
        );
        assert_eq!(
            r.redact("Authorization: Bearer abcdefghijklmnopqrstu"),
            "Authorization: [REDACTED]"
        );
        assert!(
            matches!(r.redact("nothing here"), Cow::Borrowed(_)),
            "no allocation when clean"
        );
    }

    #[test]
    fn known_secret_is_redacted_in_percent_and_base64_encodings() {
        let redactor = Redactor::new(["fleet-secret+/=?".to_owned()]);
        for text in [
            "fleet-secret%2B%2F%3D%3F",
            "fleet-secret%2b%2f%3d%3f",
            "ZmxlZXQtc2VjcmV0Ky89Pw==",
            "ZmxlZXQtc2VjcmV0Ky89Pw",
        ] {
            assert_eq!(redactor.redact(text), MARKER, "{text}");
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_owned()).collect()
    }

    fn safe_value(name: &str) -> bool {
        matches!(name, "--format" | "--device" | "--output")
    }

    fn boolean(name: &str) -> bool {
        matches!(name, "--flatten-debug-output" | "-c")
    }

    fn env(name: &str) -> bool {
        matches!(name, "-e" | "--env")
    }

    #[test]
    fn unknown_flag_values_are_redacted_and_known_flags_keep_theirs() {
        let argv = words(&[
            "maestro",
            "test",
            "--api-key",
            "k-12345678",
            "--format",
            "junit",
            "--device=emulator-5554",
            "--mystery=hunter22-value",
            "--flatten-debug-output",
            "flow.yaml",
        ]);
        assert_eq!(
            redact_argv(&argv, safe_value, boolean, env),
            words(&[
                "maestro",
                "test",
                "--api-key",
                MARKER,
                "--format",
                "junit",
                "--device=emulator-5554",
                "--mystery=[REDACTED]",
                "--flatten-debug-output",
                "flow.yaml",
            ])
        );
    }

    #[test]
    fn env_values_are_rewritten_to_key_and_marker() {
        let argv = words(&[
            "maestro",
            "test",
            "-e",
            "PIN=1234",
            "--env=TOKEN=abc",
            "-e",
            "FLAG",
        ]);
        assert_eq!(
            redact_argv(&argv, safe_value, boolean, env),
            words(&[
                "maestro",
                "test",
                "-e",
                "PIN=[REDACTED]",
                "--env=TOKEN=[REDACTED]",
                "-e",
                "[REDACTED]",
            ])
        );
    }

    #[test]
    fn an_unknown_flag_without_a_value_does_not_swallow_the_next_flag() {
        let argv = words(&[
            "maestro",
            "test",
            "--mystery",
            "--format",
            "junit",
            "--",
            "-x",
        ]);
        assert_eq!(
            redact_argv(&argv, safe_value, boolean, env),
            words(&[
                "maestro",
                "test",
                "--mystery",
                "--format",
                "junit",
                "--",
                "-x"
            ])
        );
    }

    #[test]
    fn longer_secrets_win_over_contained_ones() {
        let r = Redactor::new(["secret".to_owned(), "secret-plus".to_owned()]);
        assert_eq!(r.redact("x secret-plus y"), "x [REDACTED] y");
    }

    proptest! {
        #[test]
        fn a_secret_never_survives(secret in "[A-Za-z0-9!@#%^&*()_+=.,-]{4,24}", before in ".{0,40}", after in ".{0,40}") {
            let r = Redactor::new([secret.clone()]);
            let text = format!("{before}{secret}{after}{secret}");
            prop_assert!(!r.redact(&text).contains(&secret));
        }
    }
}
