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

/// Rehearses `tags` (all `v*` release tags when empty), oldest first.
pub(crate) fn rehearse(root: &Path, tags: &[&str]) -> anyhow::Result<()> {
    let tags: Vec<String> = if tags.is_empty() {
        git(root, &["tag", "--list", "v*", "--sort=v:refname"])?
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        tags.iter().map(|t| (*t).to_owned()).collect()
    };
    if tags.is_empty() {
        bail!("no release tags to rehearse from (`git tag -l` lists none)");
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
