// SPDX-License-Identifier: AGPL-3.0-only
//! `cargo xtask`: repository tooling. See PLAN §11.1.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a terminal tool; its output is the user interface"
)]

mod bench;
mod db;
mod fixtures;
mod lint;
mod schema;
mod tools;
mod upgrade;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};

/// Tools installed by `setup`, always at their latest release (CI does the same).
const TOOLS: &[&str] = &[
    "cargo-nextest",
    "cargo-deny",
    // The binary sits behind the `cli` feature.
    "cargo-about --features cli",
    "sqlx-cli --no-default-features --features rustls,postgres",
    "trunk",
    "mdbook",
];

const USAGE: &str = "usage: cargo xtask <setup | tools | ci | lint | sqlx | schema [--check] | web [--release] | fixtures … | bench-overhead [--runs N] [--linger SECS] [--max-cpu-percent X] | upgrade-rehearsal [TAG…]>";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = root();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["setup"] => setup(&root),
        ["tools"] => tools::install(&root),
        ["ci"] => ci(&root),
        ["lint"] => lint::run(&root),
        ["sqlx"] => db::prepare(&root),
        ["schema"] => schema::run(&root.join("docs/src/protocol/schema"), false),
        ["schema", "--check"] => schema::run(&root.join("docs/src/protocol/schema"), true),
        ["schema", "--output-dir", dir] => schema::run(Path::new(dir), false),
        ["schema", "--output-dir", dir, "--check"] => schema::run(Path::new(dir), true),
        ["web"] => web(&root, false),
        ["web", "--release"] => web(&root, true),
        ["fixtures", rest @ ..] => fixtures::run(&root, rest),
        ["bench-overhead", rest @ ..] => bench::run(&root, rest),
        ["upgrade-rehearsal", tags @ ..] => upgrade::rehearse(&root, tags),
        _ => bail!(USAGE),
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ should sit one level below the workspace root")
        .to_path_buf()
}

fn setup(root: &Path) -> anyhow::Result<()> {
    // Installs the toolchain, components and targets from rust-toolchain.toml.
    sh(root, "rustup toolchain install")?;
    for tool in TOOLS {
        sh(root, &format!("cargo install --locked {tool}"))?;
    }
    // Tailwind and chromedriver (the browser tests) go to target/tools.
    tools::install(root)
}

fn ci(root: &Path) -> anyhow::Result<()> {
    schema::run(&root.join("docs/src/protocol/schema"), true)?;
    // The sqlx tests need Postgres 18: CI's service container, or a throwaway one locally.
    let throwaway;
    let url = if let Ok(url) = std::env::var("DATABASE_URL") {
        url
    } else {
        throwaway = db::start()?;
        throwaway.url.clone()
    };
    db::migrate(root, &url)?;
    // Builds use the committed `.sqlx/` data, exactly like the Docker image build.
    let offline = [("SQLX_OFFLINE", "true")];
    sh(root, "cargo fmt --all --check")?;
    run_env(
        root,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
        &offline,
    )?;
    // Proves the browser-side crates stay wasm-safe (PLAN §4.2).
    sh(
        root,
        "cargo clippy --locked --target wasm32-unknown-unknown \
         -p fleet-protocol -p fleet-core -p fleet-ui -p fleet-web -- -D warnings",
    )?;
    lint::run(root)?;
    // The composite action's install step, with curl, gh and tar stubbed (research 06 T18).
    sh(
        root,
        "bash examples/github-actions/fleet-agent/test-install.sh",
    )?;
    sh(root, "cargo deny --locked check")?;
    // The browser tests drive the built dashboard with chromedriver (PLAN §13).
    tools::install(root)?;
    web(root, false)?;
    run_env(
        root,
        "cargo",
        &[
            "nextest",
            "run",
            "--workspace",
            "--all-features",
            "--locked",
        ],
        &[offline[0], ("DATABASE_URL", &url)],
    )?;
    // The committed offline data matches the queries (fails when a query changed).
    let mut check = db::PREPARE.to_vec();
    check.insert(2, "--check");
    run_env(
        root,
        "cargo",
        &check,
        &[("DATABASE_URL", &url), ("SQLX_OFFLINE", "false")],
    )?;
    // nextest skips doctests; rustdoc warnings include broken intra-doc links.
    run_env(
        root,
        "cargo",
        &["test", "--doc", "--workspace", "--locked"],
        &offline,
    )?;
    run_env(
        root,
        "cargo",
        &[
            "doc",
            "--workspace",
            "--no-deps",
            "--document-private-items",
            "--locked",
        ],
        &[("RUSTDOCFLAGS", "-D warnings"), offline[0]],
    )?;
    sh(root, "mdbook build docs")?;
    println!("xtask ci: all checks passed");
    Ok(())
}

fn web(root: &Path, release: bool) -> anyhow::Result<()> {
    let mut args = vec!["build", "--locked"];
    if release {
        args.push("--release");
    }
    // Trunk's tailwind asset finds the pinned CLI from `cargo xtask setup` on PATH.
    let path = std::env::join_paths(
        std::iter::once(tools::dir(root)).chain(
            std::env::var_os("PATH")
                .iter()
                .flat_map(std::env::split_paths),
        ),
    )?;
    let path = path.to_string_lossy();
    run_env(
        &root.join("crates/fleet-web"),
        "trunk",
        &args,
        &[("PATH", &path)],
    )
}

/// Runs a command line in `dir`. Arguments are split on whitespace; none of ours contain spaces.
fn sh(dir: &Path, line: &str) -> anyhow::Result<()> {
    let mut words = line.split_whitespace();
    let program = words.next().context("empty command")?;
    run(dir, program, &words.collect::<Vec<_>>())
}

fn run(dir: &Path, program: &str, args: &[&str]) -> anyhow::Result<()> {
    run_env(dir, program, args, &[])
}

pub(crate) fn run_env(
    dir: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> anyhow::Result<()> {
    println!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .envs(env.iter().copied())
        .current_dir(dir)
        .status()
        .with_context(|| format!("running {program} (try `cargo xtask setup`)"))?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}
