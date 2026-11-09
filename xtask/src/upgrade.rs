// SPDX-License-Identifier: AGPL-3.0-only
//! `cargo xtask upgrade-rehearsal`: exports each release tag's `migrations/` from git, then runs
//! the upgrade rehearsal (`crates/fleet-store/tests/upgrade.rs`) against Postgres 18. See
//! docs/src/operator-guide.md#upgrades.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};

use crate::{db, run_env};

/// The first release the rehearsal replays. ADR-0014: before it there is no compatibility promise,
/// so the alpha tags are not upgradable and are not rehearsed.
const FIRST_REHEARSED: (u64, u64, u64) = (1, 0, 0);

/// Rehearses `tags` (all `v*` release tags when empty), oldest first, from [`FIRST_REHEARSED`] on.
/// With no such release yet, it says so and succeeds.
pub(crate) fn rehearse(root: &Path, tags: &[&str]) -> anyhow::Result<()> {
    let listed: Vec<String> = if tags.is_empty() {
        git(root, &["tag", "--list", "v*", "--sort=v:refname"])?
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        tags.iter().map(|t| (*t).to_owned()).collect()
    };
    let (tags, skipped): (Vec<String>, Vec<String>) = listed
        .into_iter()
        .partition(|t| release_version(t).is_some_and(|v| v >= FIRST_REHEARSED));
    for tag in &skipped {
        println!("skipping {tag}: rehearsal starts at v1.0.0 (ADR-0014)");
    }
    if tags.is_empty() {
        println!("no release from v1.0.0 on to rehearse: nothing to do (ADR-0014)");
        return Ok(());
    }
    let export = root.join("target/upgrade-rehearsal");
    if export.exists() {
        fs::remove_dir_all(&export).with_context(|| format!("clearing {}", export.display()))?;
    }
    for tag in &tags {
        export_migrations(root, tag, &export.join(tag))?;
    }
    // Same database choice as `cargo xtask ci`: CI's Postgres, or a throwaway one.
    let throwaway;
    let url = if let Ok(url) = std::env::var("DATABASE_URL") {
        url
    } else {
        throwaway = db::start_named("fleet-upg-pg")?;
        throwaway.url.clone()
    };
    let dir = export.to_string_lossy().into_owned();
    run_env(
        root,
        "cargo",
        &[
            "nextest",
            "run",
            "-p",
            "fleet-store",
            "--test",
            "upgrade",
            "--locked",
            "--run-ignored",
            "only",
        ],
        &[
            ("SQLX_OFFLINE", "true"),
            ("DATABASE_URL", &url),
            ("FLEET_REHEARSAL_DIR", &dir),
        ],
    )
}

/// `v<major>.<minor>.<patch>` as numbers, a pre-release counting as its release. `None` for any
/// other name.
fn release_version(tag: &str) -> Option<(u64, u64, u64)> {
    let core = tag.strip_prefix('v')?.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Writes the `.sql` files of `tag`'s `migrations/` into `dest`, byte for byte as committed.
fn export_migrations(root: &Path, tag: &str, dest: &Path) -> anyhow::Result<()> {
    let listed = git(
        root,
        &["ls-tree", "-r", "--name-only", tag, "--", "migrations"],
    )
    .with_context(|| format!("listing migrations of {tag}"))?;
    let files: Vec<&str> = listed
        .lines()
        .filter(|p| Path::new(p).extension().is_some_and(|ext| ext == "sql"))
        .collect();
    if files.is_empty() {
        bail!("{tag} has no migrations/*.sql");
    }
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    for path in files {
        let bytes = git_bytes(root, &["show", &format!("{tag}:{path}")])?;
        let name = Path::new(path)
            .file_name()
            .context("a migration path should have a file name")?;
        let target: PathBuf = dest.join(name);
        fs::write(&target, bytes).with_context(|| format!("writing {}", target.display()))?;
    }
    println!("exported {} migrations of {tag}", dest.read_dir()?.count());
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    String::from_utf8(git_bytes(root, args)?).context("git printed non-UTF-8 output")
}

fn git_bytes(root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::release_version;

    #[test]
    fn only_releases_from_v1_on_are_rehearsed() {
        assert_eq!(release_version("v1.0.0"), Some((1, 0, 0)));
        assert_eq!(release_version("v1.2.3-rc.1"), Some((1, 2, 3)));
        assert!(release_version("v0.1.0-alpha.3").unwrap() < (1, 0, 0));
        assert_eq!(release_version("release-notes"), None);
    }
}
