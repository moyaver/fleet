// SPDX-License-Identifier: AGPL-3.0-only
//! Small text helpers for what the screens show.

/// A commit id shortened to 7 characters.
#[must_use]
pub fn short_commit(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// A git ref as people say it: `refs/heads/main` is `main`, `refs/tags/v1` is `v1`; other refs
/// (`refs/pull/7/merge`) stay as they are.
#[must_use]
pub fn ref_name(git_ref: &str) -> &str {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
}

/// `linux/x86_64` from what the agent reported; `None` when it reported neither.
#[must_use]
pub fn platform(os: Option<&str>, arch: Option<&str>) -> Option<String> {
    match (os, arch) {
        (Some(os), Some(arch)) => Some(format!("{os}/{arch}")),
        (Some(one), None) | (None, Some(one)) => Some(one.to_owned()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_is_cut_to_seven_characters_and_short_ones_stay() {
        assert_eq!(short_commit("0123456789abcdef"), "0123456");
        assert_eq!(short_commit("abc"), "abc");
        assert_eq!(
            short_commit("ééééééééé"),
            "ééééééé",
            "by characters, not bytes"
        );
    }

    #[test]
    fn heads_and_tags_lose_their_prefix_and_other_refs_stay() {
        assert_eq!(ref_name("refs/heads/feat/x"), "feat/x");
        assert_eq!(ref_name("refs/tags/v1.2"), "v1.2");
        assert_eq!(ref_name("refs/pull/7/merge"), "refs/pull/7/merge");
        assert_eq!(ref_name("main"), "main");
    }

    #[test]
    fn the_platform_joins_what_was_reported() {
        assert_eq!(
            platform(Some("linux"), Some("x86_64")).as_deref(),
            Some("linux/x86_64")
        );
        assert_eq!(platform(Some("macos"), None).as_deref(), Some("macos"));
        assert_eq!(platform(None, Some("arm64")).as_deref(), Some("arm64"));
        assert_eq!(platform(None, None), None);
    }
}
