//! Self-update via GitHub Releases.
//!
//! aish ships as a single binary, so "upgrade" means: find the newest published
//! GitHub release, and — if it's newer than the running build and carries an
//! asset for this platform — download that asset, extract the `aish` binary, and
//! atomically swap it over the running executable. All release I/O goes through
//! the `gh` CLI (the task's "gh release function"): `gh release view` to
//! discover the latest tag + assets, `gh release download` to fetch the asset.
//! Using `gh` means we inherit the user's existing GitHub auth and never have to
//! hold a token ourselves.
//!
//! Asset format: the release workflow publishes the per-platform binary as a
//! RAW executable (`aish-<triple>`), not an archive. For resilience the updater
//! also accepts a gzip tarball (`*.tar.gz` / `*.tgz` / `*.tar`) and extracts the
//! `aish` binary from it — so older or differently-packaged releases keep
//! working. The packaging form is decided by the asset's filename extension.
//!
//! The check is best-effort and SILENT on failure: no `gh`, no network, no
//! releases, or no matching asset all resolve to "no update available" with no
//! noise — auto-update must never get in the way of starting a shell.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default upstream repo. Overridable with `AISH_UPDATE_REPO=owner/name` (handy
/// for forks and for testing against a staging repo).
const DEFAULT_REPO: &str = "LightHeart-Ventures/aish";

fn repo() -> String {
    std::env::var("AISH_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// The version compiled into this binary (Cargo package version, e.g.
/// `0.4.0-dev`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A newer release that has an installable asset for this platform.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    /// The git tag, as published (e.g. `v0.4.1`).
    pub tag: String,
    /// Normalized numeric version (e.g. `0.4.1`).
    pub version: String,
    /// The release asset that matches this platform — either a raw binary
    /// (`aish-<triple>`) or a gzip tarball (`*.tar.gz`).
    pub asset_name: String,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
}

#[derive(Deserialize)]
struct GhRelease {
    #[serde(rename = "tagName")]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// Rust target triples whose release asset would run on this host, most-preferred
/// first. Asset names embed the triple (e.g. `aish-x86_64-unknown-linux-gnu` or
/// `aish-v0.3.0-x86_64-unknown-linux-gnu.tar.gz`), so we match on substring.
/// An empty list means "unknown platform" → we won't claim any asset fits.
fn target_triples() -> &'static [&'static str] {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => &["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
        ("aarch64", "linux") => &["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        ("x86_64", "macos") => &["x86_64-apple-darwin"],
        ("aarch64", "macos") => &["aarch64-apple-darwin"],
        _ => &[],
    }
}

/// True when an asset filename names a gzip tarball we should run through `tar`.
/// Anything else (notably the raw `aish-<triple>` binary) is treated as the
/// `aish` executable directly.
fn is_tarball(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".tar.gz") || n.ends_with(".tgz") || n.ends_with(".tar")
}

/// Pick the asset matching this platform from a release's asset list. Prefers
/// triples in `target_triples()` order; returns the first asset whose name
/// contains a matching triple. Checksum sidecars (`*.sha256`) are skipped so we
/// never mistake `aish-<triple>.sha256` for the binary itself.
fn match_asset<'a>(assets: &'a [GhAsset]) -> Option<&'a str> {
    for triple in target_triples() {
        if let Some(a) = assets
            .iter()
            .find(|a| a.name.contains(triple) && !a.name.ends_with(".sha256"))
        {
            return Some(&a.name);
        }
    }
    None
}

/// Parse a version string into `(major, minor, patch)`, ignoring a leading `v`
/// and any pre-release/build suffix (`-dev`, `+build`). Returns `None` if the
/// three numeric components can't be read.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    // Drop any pre-release (`-rc1`) or build (`+abc`) metadata.
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// True when `latest` is a strictly newer release than `current`. A pre-release
/// `current` (e.g. `0.4.0-dev`) at the SAME numeric version as `latest` is NOT
/// upgraded — a dev build is considered at-or-ahead of the equal-numbered
/// release.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        // If we can't parse one side, only upgrade on a different string.
        _ => latest.trim_start_matches('v') != current.trim_start_matches('v'),
    }
}

/// Whether `gh` is on PATH at all. A cheap gate so the rest of the flow can
/// assume it's present (and so the startup check stays silent without it).
pub fn gh_available() -> bool {
    std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Query the latest GitHub release and decide whether it's an upgrade for this
/// platform. Returns `Ok(None)` for "you're up to date / nothing installable",
/// and `Err` only for a genuine failure the caller might want to surface (used
/// by the on-demand `:update` path; the startup check swallows errors).
pub async fn check() -> Result<Option<UpdateInfo>> {
    let repo = repo();
    let out = tokio::process::Command::new("gh")
        .args([
            "release",
            "view",
            "--repo",
            &repo,
            "--json",
            "tagName,assets",
        ])
        .output()
        .await
        .context("running `gh release view` (is the GitHub CLI installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // No releases yet is a normal, quiet state — not an error to surface.
        if err.contains("release not found") || err.contains("no releases") {
            return Ok(None);
        }
        bail!("gh release view failed: {}", err.trim());
    }
    let release: GhRelease =
        serde_json::from_slice(&out.stdout).context("parsing `gh release view` JSON")?;

    if !is_newer(&release.tag_name, current_version()) {
        return Ok(None);
    }
    let Some(asset) = match_asset(&release.assets) else {
        // Newer release exists but ships no asset for this OS/arch — nothing to do.
        return Ok(None);
    };
    let version = release.tag_name.trim_start_matches('v').to_string();
    Ok(Some(UpdateInfo {
        tag: release.tag_name.clone(),
        version,
        asset_name: asset.to_string(),
    }))
}

/// Download the release asset, extract the `aish` binary, and atomically replace
/// the running executable. Prints brief progress to stdout. On macOS the freshly
/// installed binary is re-signed with an ad-hoc signature (matching the Makefile)
/// so AMFI doesn't SIGKILL it on next launch.
pub async fn perform(info: &UpdateInfo) -> Result<()> {
    let repo = repo();
    let current_exe = std::env::current_exe().context("locating the running aish binary")?;
    let current_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);

    // Fail fast with a clear message if we can't write where the binary lives
    // (e.g. installed under /usr/local by root). Better than a cryptic rename
    // error after a 25 MB download.
    let dest_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("can't determine the directory of {}", current_exe.display()))?;
    writable_check(dest_dir)
        .with_context(|| format!("{} is not writable", dest_dir.display()))?;

    // Scratch dir for the download + extraction; auto-cleaned on drop via the
    // explicit remove at the end (best-effort).
    let work = std::env::temp_dir().join(format!("aish-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("creating the update scratch dir")?;

    println!("\x1b[2mdownloading {} …\x1b[0m", info.asset_name);
    let dl = tokio::process::Command::new("gh")
        .args([
            "release",
            "download",
            &info.tag,
            "--repo",
            &repo,
            "--pattern",
            &info.asset_name,
            "--dir",
            &work.to_string_lossy(),
            "--clobber",
        ])
        .output()
        .await
        .context("running `gh release download`")?;
    if !dl.status.success() {
        let _ = std::fs::remove_dir_all(&work);
        bail!(
            "gh release download failed: {}",
            String::from_utf8_lossy(&dl.stderr).trim()
        );
    }

    let downloaded = work.join(&info.asset_name);
    if !downloaded.exists() {
        let _ = std::fs::remove_dir_all(&work);
        bail!("downloaded asset not found at {}", downloaded.display());
    }

    // The release ships the platform binary either as a raw executable
    // (`aish-<triple>`, the current format) or — for resilience against older /
    // differently-packaged releases — as a gzip tarball. Decide which by the
    // asset's filename extension and pull out the `aish` binary accordingly.
    let new_bin = if is_tarball(&info.asset_name) {
        println!("\x1b[2munpacking …\x1b[0m");
        let untar = tokio::process::Command::new("tar")
            .args([
                "-xzf",
                &downloaded.to_string_lossy(),
                "-C",
                &work.to_string_lossy(),
            ])
            .output()
            .await
            .context("running tar to unpack the release")?;
        if !untar.status.success() {
            let _ = std::fs::remove_dir_all(&work);
            bail!("tar failed: {}", String::from_utf8_lossy(&untar.stderr).trim());
        }
        match find_binary(&work, "aish") {
            Some(p) => p,
            None => {
                let _ = std::fs::remove_dir_all(&work);
                bail!("no `aish` binary found inside {}", info.asset_name);
            }
        }
    } else {
        // Raw binary asset — the downloaded file *is* the new aish.
        downloaded.clone()
    };

    // Stage the new binary alongside the destination (same filesystem, so the
    // final rename is atomic), make it executable, then re-sign on macOS.
    let staged = dest_dir.join(format!(".aish-update-{}", std::process::id()));
    std::fs::copy(&new_bin, &staged)
        .with_context(|| format!("staging new binary at {}", staged.display()))?;
    set_executable(&staged)?;
    if cfg!(target_os = "macos") {
        let signed = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&staged)
            .status();
        match signed {
            Ok(s) if s.success() => {}
            _ => eprintln!(
                "\x1b[33mwarning:\x1b[0m codesign failed — if the new aish is killed on launch, run `codesign --force --sign - {}`",
                current_exe.display()
            ),
        }
    }

    // Atomic swap: rename the staged binary over the running one. On Unix the
    // running process keeps executing the old (now-unlinked) inode; the new
    // binary takes effect on the next launch.
    std::fs::rename(&staged, &current_exe).with_context(|| {
        format!("replacing {} with the new binary", current_exe.display())
    })?;

    let _ = std::fs::remove_dir_all(&work);
    println!(
        "\x1b[32m✓\x1b[0m upgraded to aish {} — restart aish to use it.",
        info.version
    );
    Ok(())
}

/// Confirm a directory is writable by creating and removing a probe file.
fn writable_check(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".aish-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Set the executable bit (0o755) on a freshly-staged binary.
fn set_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}

/// Recursively search `root` for a regular file named `name` (the extracted
/// archive may place the binary at the top level or under a versioned subdir).
fn find_binary(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for e in entries.flatten() {
            let path = e.path();
            let ty = e.file_type().ok()?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && e.file_name().to_string_lossy() == name {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_and_without_prefix_and_suffix() {
        assert_eq!(parse_semver("v0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_semver("0.4.0-dev"), Some((0, 4, 0)));
        assert_eq!(parse_semver("1.2.3+build7"), Some((1, 2, 3)));
        assert_eq!(parse_semver("v2"), Some((2, 0, 0)));
        assert_eq!(parse_semver("2.5"), Some((2, 5, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("v0.4.0", "0.3.0"));
        assert!(is_newer("v0.3.1", "v0.3.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        // Same numeric version: not newer (a dev build is at-or-ahead).
        assert!(!is_newer("v0.4.0", "0.4.0-dev"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        // Older release than the running build → no downgrade.
        assert!(!is_newer("v0.2.0", "0.4.0-dev"));
    }

    #[test]
    fn matches_asset_for_known_triples() {
        let assets = vec![
            GhAsset { name: "aish-v0.3.0-x86_64-unknown-linux-gnu.tar.gz".into() },
            GhAsset { name: "aish-v0.3.0-aarch64-apple-darwin.tar.gz".into() },
        ];
        // On any supported platform we either match one of these or (unknown
        // platform) match none — never panic, never a wrong-arch pick.
        if let Some(name) = match_asset(&assets) {
            assert!(target_triples().iter().any(|t| name.contains(t)));
        } else {
            assert!(target_triples().is_empty());
        }
    }

    #[test]
    fn matches_raw_binary_assets() {
        // The current release format: raw per-platform binaries with no
        // extension, each accompanied by a `.sha256` sidecar.
        let assets = vec![
            GhAsset { name: "aish-x86_64-unknown-linux-gnu".into() },
            GhAsset { name: "aish-x86_64-unknown-linux-gnu.sha256".into() },
            GhAsset { name: "aish-aarch64-apple-darwin".into() },
            GhAsset { name: "aish-aarch64-apple-darwin.sha256".into() },
            GhAsset { name: "aish-x86_64-apple-darwin".into() },
            GhAsset { name: "aish-x86_64-apple-darwin.sha256".into() },
            GhAsset { name: "SHA256SUMS".into() },
        ];
        if let Some(name) = match_asset(&assets) {
            // Never pick a checksum sidecar, always a real per-platform binary.
            assert!(!name.ends_with(".sha256"));
            assert!(target_triples().iter().any(|t| name.contains(t)));
        } else {
            assert!(target_triples().is_empty());
        }
    }

    #[test]
    fn never_matches_a_checksum_sidecar() {
        // A sidecar present without its binary must NOT be selected.
        let assets = vec![GhAsset {
            name: "aish-aarch64-apple-darwin.sha256".into(),
        }];
        assert!(match_asset(&assets).is_none());
    }

    #[test]
    fn tarball_detection() {
        assert!(is_tarball("aish-v0.3.0-x86_64-apple-darwin.tar.gz"));
        assert!(is_tarball("aish.TGZ"));
        assert!(is_tarball("bundle.tar"));
        // Raw binaries and checksum sidecars are not tarballs.
        assert!(!is_tarball("aish-aarch64-apple-darwin"));
        assert!(!is_tarball("aish-aarch64-apple-darwin.sha256"));
    }

    #[test]
    fn no_asset_when_none_match() {
        let assets = vec![GhAsset { name: "aish-v0.3.0-sparc64-unknown-haiku.tar.gz".into() }];
        assert!(match_asset(&assets).is_none());
    }
}
