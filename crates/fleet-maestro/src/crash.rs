// SPDX-License-Identifier: AGPL-3.0-only
//! An app crash during a flow, from Maestro's `logs/crash-report.txt` (issue #75).
//!
//! Maestro writes the file when the app under test dies while the flow runs. Its lines are the
//! device's `AndroidRuntime` log lines, in `logcat -v time` form:
//!
//! ```text
//! 11-07 01:41:27.425 E/AndroidRuntime(10110): Process: com.android.settings, PID: 10110
//! 11-07 01:41:27.425 E/AndroidRuntime(10110): android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash
//! 11-07 01:41:27.425 E/AndroidRuntime(10110): \tat android.app.ActivityThread.throwRemoteServiceException(…)
//! ```
//!
//! The crash is evidence on the flow. It does not change the flow's verdict, which stays Maestro's
//! own (here, the failed assertion). The fixture the rule was written from is
//! `fixtures/maestro/2.12.1/crash`; its logcat has no `FATAL EXCEPTION` lines (the import cut the
//! middle), so the crash report is the only source this rule reads.

/// The app that died, and the exception that killed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCrash {
    /// The app's package, for example `com.android.settings`.
    pub package: String,
    /// The exception's first line: its class, then its message after a colon.
    pub exception: String,
}

/// The crash `text` (a `crash-report.txt`) records, or `None` when it names no process.
///
/// The package comes from the `Process: <package>, PID: <pid>` line. The exception is the first
/// `AndroidRuntime` message after it that is not a stack frame (stack frames are indented or start
/// with `at `) and not empty.
#[must_use]
pub fn parse_crash_report(text: &str) -> Option<AppCrash> {
    let mut package = None;
    for line in text.lines() {
        let Some(message) = runtime_message(line) else {
            continue;
        };
        match &package {
            None => {
                if let Some(rest) = message.strip_prefix("Process: ") {
                    package = rest.split(',').next().map(|p| p.trim().to_owned());
                }
            }
            Some(package) => {
                if message.is_empty()
                    || message.starts_with(char::is_whitespace)
                    || message.starts_with("at ")
                {
                    continue;
                }
                return Some(AppCrash {
                    package: package.clone(),
                    exception: message.to_owned(),
                });
            }
        }
    }
    None
}

/// The message of an `AndroidRuntime` line: the text after `AndroidRuntime(<pid>): `.
fn runtime_message(line: &str) -> Option<&str> {
    let (_, after_tag) = line.split_once("AndroidRuntime(")?;
    let (_, message) = after_tag.split_once("): ")?;
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crash fixture's report, as Maestro wrote it.
    const CRASH: &str = include_str!(
        "../../../fixtures/maestro/2.12.1/crash/run/debug/crash/logs/crash-report.txt"
    );

    #[test]
    fn the_fixture_crash_names_the_package_and_the_exception() {
        assert_eq!(
            parse_crash_report(CRASH),
            Some(AppCrash {
                package: "com.android.settings".into(),
                exception:
                    "android.app.RemoteServiceException$CrashedByAdbException: shell-induced crash"
                        .into(),
            })
        );
    }

    #[test]
    fn a_report_without_a_process_names_no_crash() {
        assert_eq!(parse_crash_report(""), None);
        assert_eq!(
            parse_crash_report("11-07 01:00:00.000 I/adbd: nothing to see"),
            None
        );
    }

    #[test]
    fn stack_frames_are_not_the_exception() {
        // A process line with only frames after it names no exception.
        let frames = "11-07 01:00:00.000 E/AndroidRuntime(1): Process: a.b, PID: 1\n\
                      11-07 01:00:00.000 E/AndroidRuntime(1): \tat a.b.C.d(C.java:1)\n\
                      11-07 01:00:00.000 E/AndroidRuntime(1): at a.b.C.e(C.java:2)\n";
        assert_eq!(parse_crash_report(frames), None);
    }
}
