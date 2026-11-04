// SPDX-License-Identifier: AGPL-3.0-only
//! `cargo xtask`: repository tooling. See PLAN §11.1.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "xtask is a terminal tool; its output is the user interface"
)]

mod lint;

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
    "trunk",
    "mdbook",
];

const USAGE: &str = "usage: cargo xtask <setup | ci | lint | web [--release]>";

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
        ["ci"] => ci(&root),
        ["lint"] => lint::run(&root),
        ["web"] => web(&root, false),
        ["web", "--release"] => web(&root, true),
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
    Ok(())
}

fn ci(root: &Path) -> anyhow::Result<()> {
    sh(root, "cargo fmt --all --check")?;
    sh(
        root,
        "cargo clippy --workspace --all-targets --locked -- -D warnings",
    )?;
    // Proves the browser-side crates stay wasm-safe (PLAN §4.2).
    sh(
        root,
        "cargo clippy --locked --target wasm32-unknown-unknown \
         -p fleet-protocol -p fleet-core -p fleet-ui -p fleet-web -- -D warnings",
    )?;
    lint::run(root)?;
    sh(root, "cargo deny --locked check")?;
    sh(root, "cargo nextest run --workspace --locked")?;
    // nextest skips doctests; rustdoc warnings include broken intra-doc links.
    sh(root, "cargo test --doc --workspace --locked")?;
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
        &[("RUSTDOCFLAGS", "-D warnings")],
    )?;
    web(root, false)?;
    sh(root, "mdbook build docs")?;
    println!("xtask ci: all checks passed");
    Ok(())
}

fn web(root: &Path, release: bool) -> anyhow::Result<()> {
    let flag = if release { " --release" } else { "" };
    sh(
        &root.join("crates/fleet-web"),
        &format!("trunk build --locked{flag}"),
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

fn run_env(dir: &Path, program: &str, args: &[&str], env: &[(&str, &str)]) -> anyhow::Result<()> {
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
