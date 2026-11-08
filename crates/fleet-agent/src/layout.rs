// SPDX-License-Identifier: AGPL-3.0-only
//! Where this machine's Maestro will write, for the command as given (research 01 §4.1, issue #148).
//!
//! The pure rules are in `fleet_maestro::invocation`. This module supplies what they need from the
//! machine: the directory the JVM calls `user.home`, and the workspace config's `testOutputDir`,
//! which Maestro reads from the `--config` file or from a `config.yaml` beside a directory input.

use std::path::{Path, PathBuf};

use fleet_maestro::invocation::{parse, user_home, workspace_test_output_dir};

/// The `user.home` Maestro's JVM will see: `-Duser.home` in this process's `JAVA_OPTS` or
/// `MAESTRO_OPTS` (the child inherits them), else the account's home directory. `HOME` is not
/// consulted, as the JVM does not read it.
pub(crate) fn user_home_here() -> Option<PathBuf> {
    let account = nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map(|u| u.dir);
    user_home(
        std::env::var("JAVA_OPTS").ok().as_deref(),
        std::env::var("MAESTRO_OPTS").ok().as_deref(),
        account.as_deref(),
    )
}

/// The `testOutputDir` of the workspace config Maestro will use for `argv`, relative to `cwd` as
/// Maestro resolves it. The config is `--config`, else the first `config.yaml` or `config.yml` in a
/// directory the command names (captured: `paths-config-*`). A file input does not read the config
/// beside it. `None` when there is none, or it sets no directory.
pub(crate) fn workspace_test_dir(argv: &[String], cwd: Option<&Path>) -> Option<String> {
    let inv = parse(argv);
    let config = match &inv.config {
        Some(path) => Some(resolve(cwd, Path::new(path))),
        None => inv
            .positionals
            .iter()
            .map(|p| resolve(cwd, Path::new(p)))
            .filter(|p| p.is_dir())
            .find_map(|dir| {
                ["config.yaml", "config.yml"]
                    .iter()
                    .map(|name| dir.join(name))
                    .find(|c| c.is_file())
            }),
    }?;
    let text = std::fs::read_to_string(config).ok()?;
    workspace_test_output_dir(&text)
}

fn resolve(cwd: Option<&Path>, path: &Path) -> PathBuf {
    match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path.to_path_buf(),
    }
}
