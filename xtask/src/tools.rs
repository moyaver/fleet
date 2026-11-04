// SPDX-License-Identifier: AGPL-3.0-only
//! Native tools the dashboard build and browser tests need, downloaded into `target/tools/`
//! by `cargo xtask setup` (PLAN §5.4): the Tailwind CSS standalone CLI and chromedriver.
//! No Node anywhere (I9).

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

/// The pinned Tailwind CSS release; its binaries are verified against the digests below, taken
/// from the release's `sha256sums.txt`.
const TAILWIND_VERSION: &str = "4.3.4";

/// `(platform, sha256)` of `tailwindcss-<platform>` in release [`TAILWIND_VERSION`].
const TAILWIND_SHA256: &[(&str, &str)] = &[
    (
        "macos-arm64",
        "a516a11b439bfbb5ecd4a2d13b75e6cdcc284dc710aed8731e31d2e5c03895a4",
    ),
    (
        "macos-x64",
        "7ec771593a1fb8bbd204f70429361b0158f7989e4136bc68d04513bf543a15ba",
    ),
    (
        "linux-x64",
        "149a8ece5b855587a9c10de6da9dcbcfad0e47c4ed5dca219f721b3eae39d817",
    ),
    (
        "linux-arm64",
        "f6d7baae1d350ed3a2defe98d0b5cc58e37400c0173b2e38d42f690ccc4bbfe1",
    ),
];

const CHROME_FOR_TESTING: &str = "https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json";

/// Where the tools live.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("target/tools")
}

/// Installs Tailwind and chromedriver unless they are already in place.
pub(crate) fn install(root: &Path) -> anyhow::Result<()> {
    let dir = dir(root);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    tailwind(&dir)?;
    chromedriver(&dir)
}

/// `macos-arm64`, `linux-x64`, …: the naming Tailwind uses.
fn platform() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        (os, arch) => bail!("no Tailwind or chromedriver build for {os}/{arch}"),
    })
}

fn curl(url: &str, out: &Path) -> anyhow::Result<()> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--output",
        ])
        .arg(out)
        .arg(url)
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("downloading {url} failed: {status}");
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hex = String::new();
    for byte in Sha256::digest(bytes) {
        write!(hex, "{byte:02x}").expect("writing to a String should not fail");
    }
    Ok(hex)
}

fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

fn tailwind(dir: &Path) -> anyhow::Result<()> {
    let target = dir.join("tailwindcss");
    let platform = platform()?;
    let expected = TAILWIND_SHA256
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, sum)| *sum)
        .with_context(|| format!("no pinned Tailwind checksum for {platform}"))?;
    if target.is_file() && sha256_hex(&target)? == expected {
        return Ok(());
    }
    let url = format!(
        "https://github.com/tailwindlabs/tailwindcss/releases/download/v{TAILWIND_VERSION}/tailwindcss-{platform}"
    );
    println!("fetching Tailwind CSS {TAILWIND_VERSION} ({platform})");
    let partial = dir.join("tailwindcss.partial");
    curl(&url, &partial)?;
    let actual = sha256_hex(&partial)?;
    if actual != expected {
        fs::remove_file(&partial).ok();
        bail!("Tailwind checksum mismatch for {platform}: expected {expected}, got {actual}");
    }
    make_executable(&partial)?;
    fs::rename(&partial, &target).context("installing tailwindcss")
}

/// The Chrome the tests will drive: `$CHROME`, then the usual install locations.
fn chrome_binary() -> anyhow::Result<PathBuf> {
    let mut candidates = Vec::new();
    candidates.extend(std::env::var_os("CHROME").map(PathBuf::from));
    candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    for name in ["google-chrome", "google-chrome-stable", "chromium"] {
        candidates.extend(
            std::env::var_os("PATH")
                .iter()
                .flat_map(std::env::split_paths)
                .map(|d| d.join(name)),
        );
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("Chrome not found: install it or set CHROME")
}

/// The major version in `Google Chrome 156.0.8078.81 `.
fn major_version(output: &str) -> Option<&str> {
    output
        .split_whitespace()
        .find_map(|word| word.split_once('.').map(|(major, _)| major))
        .filter(|major| major.chars().all(|c| c.is_ascii_digit()))
}

fn chromedriver(dir: &Path) -> anyhow::Result<()> {
    let target = dir.join("chromedriver");
    let chrome = chrome_binary()?;
    let out = Command::new(&chrome)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", chrome.display()))?;
    let version = String::from_utf8_lossy(&out.stdout).into_owned();
    let major = major_version(&version)
        .with_context(|| format!("unexpected Chrome version output: {version:?}"))?
        .to_owned();
    let stamp = dir.join("chromedriver.milestone");
    if target.is_file() && fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == major) {
        return Ok(());
    }
    // chromedriver must match Chrome's milestone.
    let index = dir.join("chrome-for-testing.json");
    curl(CHROME_FOR_TESTING, &index)?;
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&index)?)
        .context("parsing the Chrome for Testing index")?;
    let platform = match platform()? {
        "macos-arm64" => "mac-arm64",
        "macos-x64" => "mac-x64",
        "linux-x64" => "linux64",
        other => bail!("Chrome for Testing has no chromedriver for {other}"),
    };
    let url = json["milestones"][&major]["downloads"]["chromedriver"]
        .as_array()
        .and_then(|all| all.iter().find(|d| d["platform"] == platform))
        .and_then(|d| d["url"].as_str())
        .with_context(|| format!("no chromedriver {platform} for Chrome {major}"))?
        .to_owned();
    println!("fetching chromedriver for Chrome {major} ({platform})");
    let zip = dir.join("chromedriver.zip");
    curl(&url, &zip)?;
    let unpacked = dir.join("chromedriver-unpacked");
    fs::remove_dir_all(&unpacked).ok();
    let status = Command::new("unzip")
        .args(["-q", "-o"])
        .arg(&zip)
        .arg("-d")
        .arg(&unpacked)
        .status()
        .context("running unzip")?;
    if !status.success() {
        bail!("unzip failed: {status}");
    }
    let binary = unpacked.join(format!("chromedriver-{platform}/chromedriver"));
    make_executable(&binary)?;
    fs::rename(&binary, &target).context("installing chromedriver")?;
    fs::write(&stamp, &major).context("recording the chromedriver milestone")?;
    fs::remove_dir_all(&unpacked).ok();
    fs::remove_file(&zip).ok();
    fs::remove_file(&index).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::major_version;

    #[test]
    fn major_version_is_read_from_chrome_output() {
        assert_eq!(major_version("Google Chrome 156.0.8078.81 \n"), Some("156"));
        assert_eq!(major_version("Chromium 131.0.6778.204 snap"), Some("131"));
        assert_eq!(major_version("no digits here"), None);
    }
}
