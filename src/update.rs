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
use serde::{Deserialize, Serialize};
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

/// Release channels `:update` can track. A channel decides HOW the target
/// release is discovered (see [`resolve_release`]) while every channel funnels
/// into the SAME download/verify/apply path ([`perform`]):
///
/// * [`Channel::Prod`] — stable `v{semver}` releases marked "latest". Discovered
///   with `gh release view` (no tag ⇒ the repo's latest published release). This
///   is the historical, backward-compatible behaviour and the default.
/// * [`Channel::Dev`] — nightly pre-releases tagged `dev-v{next}-dev.{n}`.
/// * [`Channel::Ci`] — per-main-push pre-releases tagged `ci-{run}-{sha}`.
///
/// Dev/Ci tags are NOT strict semver, so they can't be discovered via
/// `gh release view` (which only knows "latest"); instead we `gh release list`
/// and client-side filter by the channel's tag prefix, newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Stable `v{semver}` releases (default).
    Prod,
    /// Nightly `dev-v{next}-dev.{n}` pre-releases.
    Dev,
    /// Per-commit `ci-{run}-{sha}` pre-releases.
    Ci,
}

impl Channel {
    /// The tag prefix that identifies this channel's releases when scanning
    /// `gh release list`. `Prod` has no prefix (it uses the "latest" pointer).
    pub fn tag_prefix(self) -> Option<&'static str> {
        match self {
            Channel::Prod => None,
            Channel::Dev => Some("dev-"),
            Channel::Ci => Some("ci-"),
        }
    }

    /// The lowercase channel name as accepted by `AISH_UPDATE_CHANNEL`.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Prod => "prod",
            Channel::Dev => "dev",
            Channel::Ci => "ci",
        }
    }
}

/// Parse a channel name (case-insensitive). Recognises `prod`/`stable`,
/// `dev`/`nightly`, and `ci`; anything else (including unset) is `None` so the
/// caller can fall back to the default.
pub fn parse_channel(s: &str) -> Option<Channel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "prod" | "stable" | "release" => Some(Channel::Prod),
        "dev" | "nightly" => Some(Channel::Dev),
        "ci" => Some(Channel::Ci),
        _ => None,
    }
}

/// The active update channel, read from `AISH_UPDATE_CHANNEL`. Defaults to
/// [`Channel::Prod`] when the var is unset, empty, or unrecognised — existing
/// users keep tracking stable releases with zero configuration.
pub fn channel() -> Channel {
    std::env::var("AISH_UPDATE_CHANNEL")
        .ok()
        .as_deref()
        .and_then(parse_channel)
        .unwrap_or(Channel::Prod)
}

/// The version compiled into this binary (Cargo package version, e.g.
/// `0.4.0-dev`). This is automatically baked in from Cargo.toml at compile
/// time via `env!("CARGO_PKG_VERSION")`. **IMPORTANT: Always keep Cargo.toml's
/// [package] version field in sync with the current release tag to prevent
/// version detection drift during updates.**
///
/// For dev releases, `AISH_RELEASE_TAG` (if set at build time by the
/// `release-dev.yml` workflow) appends the snapshot label, making the version
/// visible as: "0.25.1 (dev snapshot dev-v0.26.0-dev.6)".
pub fn current_version() -> &'static str {
    // AISH_VERSION_STRING is generated at build time by build.rs, which composes
    // the base version with an optional dev snapshot tag from AISH_RELEASE_TAG.
    env!("AISH_VERSION_STRING")
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

/// A lightweight `gh release list` row — just the tag, used to find the newest
/// release matching a channel's prefix before we fetch its full asset list.
#[derive(Deserialize)]
struct GhReleaseSummary {
    #[serde(rename = "tagName")]
    tag_name: String,
}

/// Given tags in `gh release list` order (newest first), return the first whose
/// name starts with `prefix`. Pure so the Dev/Ci discovery filter is unit-tested
/// without hitting the network.
fn first_with_prefix<'a>(tags: &'a [String], prefix: &str) -> Option<&'a str> {
    tags.iter().map(|s| s.as_str()).find(|t| t.starts_with(prefix))
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

/// Resolve the target release for `channel` on `repo`, returning its tag + full
/// asset list (or `Ok(None)` when the channel has no matching release yet).
///
/// * `Prod` keeps the historical `gh release view` path (the repo's "latest"
///   published release) verbatim — full backward compatibility.
/// * `Dev`/`Ci` `gh release list` and client-side filter by the channel's tag
///   prefix (newest first), then `gh release view <tag>` for the asset list.
async fn resolve_release(repo: &str, channel: Channel) -> Result<Option<GhRelease>> {
    match channel.tag_prefix() {
        None => resolve_latest(repo).await,
        Some(prefix) => resolve_prefixed(repo, prefix).await,
    }
}

/// Prod discovery: `gh release view` with no tag returns the repo's latest
/// published (non-pre-release) release. Unchanged from the original `check()`.
async fn resolve_latest(repo: &str) -> Result<Option<GhRelease>> {
    let out = tokio::process::Command::new("gh")
        .args([
            "release",
            "view",
            "--repo",
            repo,
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
    Ok(Some(release))
}

/// Dev/Ci discovery: list releases, pick the newest whose tag starts with
/// `prefix`, then fetch that release's assets. Pre-releases are included by
/// `gh release list` by default, which is exactly what dev/ci channels want.
async fn resolve_prefixed(repo: &str, prefix: &str) -> Result<Option<GhRelease>> {
    let out = tokio::process::Command::new("gh")
        .args([
            "release",
            "list",
            "--repo",
            repo,
            "--limit",
            "100",
            "--json",
            "tagName",
        ])
        .output()
        .await
        .context("running `gh release list` (is the GitHub CLI installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("release not found") || err.contains("no releases") {
            return Ok(None);
        }
        bail!("gh release list failed: {}", err.trim());
    }
    let summaries: Vec<GhReleaseSummary> =
        serde_json::from_slice(&out.stdout).context("parsing `gh release list` JSON")?;
    let tags: Vec<String> = summaries.into_iter().map(|s| s.tag_name).collect();
    let Some(tag) = first_with_prefix(&tags, prefix) else {
        // Channel opted into, but no release published for it yet — quiet no-op.
        return Ok(None);
    };
    view_release(repo, tag).await.map(Some)
}

/// Fetch a specific release's tag + assets by tag name.
async fn view_release(repo: &str, tag: &str) -> Result<GhRelease> {
    let out = tokio::process::Command::new("gh")
        .args([
            "release",
            "view",
            tag,
            "--repo",
            repo,
            "--json",
            "tagName,assets",
        ])
        .output()
        .await
        .context("running `gh release view <tag>`")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("gh release view {tag} failed: {}", err.trim());
    }
    serde_json::from_slice(&out.stdout).context("parsing `gh release view <tag>` JSON")
}

/// Query the target release for the active [`channel`] and decide whether it's
/// an upgrade for this platform. Returns `Ok(None)` for "you're up to date /
/// nothing installable", and `Err` only for a genuine failure the caller might
/// want to surface (used by the on-demand `:update` path; the startup check
/// swallows errors). All three channels converge here and hand a single
/// [`UpdateInfo`] to the existing download/apply path.
///
/// This is the network (uncached) path — every successful resolve refreshes the
/// on-disk TTL cache ([`cache_path`]) as a side effect, so a manual `:update`
/// also primes the cache the next startup reads.
pub async fn check() -> Result<Option<UpdateInfo>> {
    check_channel(channel()).await
}

/// Same as [`check`] but for an explicitly-chosen [`Channel`], bypassing the
/// `AISH_UPDATE_CHANNEL` env default. Backs `:update <channel>`, letting a user
/// pull from dev/ci for a single invocation without exporting the env var.
/// Always hits the network; writes the result to the TTL cache best-effort.
pub async fn check_channel(ch: Channel) -> Result<Option<UpdateInfo>> {
    let (info, cache) = resolve_channel_network(ch).await?;
    // Best-effort cache write: a filesystem hiccup must never fail an update
    // check. The cache is a startup optimisation, not a source of truth.
    if let Some(c) = cache {
        let _ = write_cache(&cache_path(), &c);
    }
    Ok(info)
}

/// Resolve the target release over the network and split the answer into (a) the
/// caller-facing [`UpdateInfo`] (Some only when a newer release with a matching
/// asset exists) and (b) a [`CachedCheck`] snapshot of the LATEST release for
/// this channel (Some whenever a release was found, regardless of whether it's
/// an upgrade). Keeping the raw latest — not just the upgrade decision — lets a
/// later cache read re-evaluate `is_newer` against a freshly-updated
/// `current_version()` without another network round-trip.
async fn resolve_channel_network(
    ch: Channel,
) -> Result<(Option<UpdateInfo>, Option<CachedCheck>)> {
    let repo = repo();
    let Some(release) = resolve_release(&repo, ch).await? else {
        // No release published for this channel yet — nothing to cache.
        return Ok((None, None));
    };
    let version = release.tag_name.trim_start_matches('v').to_string();
    let asset = match_asset(&release.assets).map(|s| s.to_string());
    let cache = CachedCheck {
        last_check_ts: now_secs(),
        latest_tag: release.tag_name.clone(),
        latest_version: version.clone(),
        asset_name: asset.clone(),
        channel: ch.as_str().to_string(),
    };
    let info = cached_to_info(&cache, current_version());
    Ok((info, Some(cache)))
}

// ---------------------------------------------------------------------------
// TASK-248 / FR-305 — 24h TTL update-check cache
// ---------------------------------------------------------------------------
//
// The startup self-update check used to shell out to `gh release view` on EVERY
// launch — a network round-trip on the critical path of opening a shell. This
// cache eliminates ~99% of those calls: the newest-release answer is persisted
// to `~/.aish/config/update-check.json` and reused while it's younger than the
// TTL (default 24h). A miss / stale / corrupt cache falls back to the network
// path, which rewrites the cache. All of it is best-effort and silent: a cache
// or filesystem failure degrades to "check the network", never an error.

/// Default staleness window for the update-check cache: 24 hours. Overridable
/// via `AISH_UPDATE_CHECK_TTL` (whole seconds; `0` forces a fresh check).
pub const DEFAULT_UPDATE_CHECK_TTL_SECS: u64 = 24 * 60 * 60;

/// The persisted latest-release snapshot. Stores the raw latest tag/version (not
/// just the upgrade decision) plus the platform asset name resolved at check
/// time and the channel it was checked for, so a read can re-derive an
/// [`UpdateInfo`] offline and correctly go quiet once the user upgrades.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedCheck {
    /// Unix seconds when this check ran (used for the TTL comparison).
    pub last_check_ts: u64,
    /// The latest release's git tag (e.g. `v0.27.0`).
    pub latest_tag: String,
    /// Normalized numeric version (e.g. `0.27.0`).
    pub latest_version: String,
    /// The platform asset matched at check time, if any. `None` means the latest
    /// release shipped nothing installable for this OS/arch — cached so we don't
    /// re-query only to reach the same dead end.
    #[serde(default)]
    pub asset_name: Option<String>,
    /// The channel this snapshot was taken for; a read only trusts the cache when
    /// it matches the active channel (switching channels forces a fresh check).
    #[serde(default)]
    pub channel: String,
}

/// Current Unix time in whole seconds (0 on a clock error — best-effort).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The cache staleness window in seconds. Reads `AISH_UPDATE_CHECK_TTL` (whole
/// seconds); `0` is honoured verbatim (always fetch fresh). Falls back to
/// [`DEFAULT_UPDATE_CHECK_TTL_SECS`] only when the var is unset or unparseable.
pub fn check_ttl() -> u64 {
    parse_check_ttl(std::env::var("AISH_UPDATE_CHECK_TTL").ok().as_deref())
}

/// Pure parse of `AISH_UPDATE_CHECK_TTL` (whole seconds). `0` is honoured
/// verbatim (always fetch fresh); unset/unparseable falls back to
/// [`DEFAULT_UPDATE_CHECK_TTL_SECS`]. Never panics. Split out for unit tests
/// that don't mutate process env (TASK-253 / FR-305).
pub fn parse_check_ttl(v: Option<&str>) -> u64 {
    v.and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_UPDATE_CHECK_TTL_SECS)
}

/// Where the cache lives: `AISH_UPDATE_CHECK_CACHE_PATH` if set (and non-empty),
/// else `~/.aish/config/update-check.json` (alongside the local-model
/// selection, via [`crate::hwdetect::config_dir`] which creates the dir).
pub fn cache_path() -> PathBuf {
    parse_cache_path(std::env::var_os("AISH_UPDATE_CHECK_CACHE_PATH").as_deref())
        .unwrap_or_else(|| crate::hwdetect::config_dir().join("update-check.json"))
}

/// Pure parse of `AISH_UPDATE_CHECK_CACHE_PATH`: `Some(path)` when set and
/// non-empty, else `None` (caller supplies the default location). Never panics.
/// Split out for unit tests that don't mutate process env (TASK-253 / FR-305).
pub fn parse_cache_path(v: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    v.map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// Pure TTL predicate: is a cache written at `last_check_ts` still fresh at
/// `now` given `ttl` seconds? A `ttl` of 0 is never fresh (always refetch). A
/// clock that went backwards (now < last_check_ts) is treated as stale via the
/// saturating subtraction. Split out so the freshness logic is unit-tested with
/// mocked time, no filesystem or clock involved (AC1).
pub fn is_cache_fresh(now: u64, last_check_ts: u64, ttl: u64) -> bool {
    if ttl == 0 || now < last_check_ts {
        return false;
    }
    now - last_check_ts < ttl
}

/// Re-derive the caller-facing [`UpdateInfo`] from a cached snapshot against a
/// given `current` version. Returns `Some` only when the cached latest is
/// strictly newer than `current` AND a platform asset was recorded — matching
/// the live [`check_channel`] semantics, but with zero network. Pure over
/// `current` so upgrade-decision behaviour is unit-tested without env/globals.
fn cached_to_info(c: &CachedCheck, current: &str) -> Option<UpdateInfo> {
    let asset = c.asset_name.clone()?;
    if is_newer(&c.latest_tag, current) {
        Some(UpdateInfo {
            tag: c.latest_tag.clone(),
            version: c.latest_version.clone(),
            asset_name: asset,
        })
    } else {
        None
    }
}

/// Read + parse the cache at `path`. `None` on a missing, unreadable, or corrupt
/// file — a corrupt cache is indistinguishable from a miss and both fall back to
/// the network (AC2). Pure over the path so tests point it at a temp file.
pub fn read_cache(path: &Path) -> Option<CachedCheck> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write the cache to `path` (pretty JSON), creating the parent directory if
/// needed. Returns `Err` only so callers can log; every call site treats a
/// write failure as non-fatal (the cache is an optimisation).
pub fn write_cache(path: &Path, cache: &CachedCheck) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(cache).context("serializing update-check cache")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Cache-aware startup update check for the active [`channel`]. This is the
/// entry point the REPL spawns off the startup critical path.
///
/// Fast path (AC1): when the cache is younger than the TTL and was taken for the
/// active channel, the answer is served from disk with NO network call. Slow
/// path (AC2): a miss / stale / corrupt cache — or `TTL=0` (AC3) — falls through
/// to [`check_channel`], which hits the network and rewrites the cache. Silent /
/// best-effort throughout (AC4): the caller swallows any `Err`.
pub async fn check_cached() -> Result<Option<UpdateInfo>> {
    let ch = channel();
    let ttl = check_ttl();
    if ttl > 0 {
        if let Some(c) = read_cache(&cache_path()) {
            if c.channel == ch.as_str() && is_cache_fresh(now_secs(), c.last_check_ts, ttl) {
                // Fresh cache hit for this channel — serve offline, no `gh`.
                return Ok(cached_to_info(&c, current_version()));
            }
        }
    }
    // Miss / stale / corrupt / TTL=0 — fetch fresh (and rewrite the cache).
    check_channel(ch).await
}

/// Format a byte count for human eyes (`24.7 MB`, `512 B`). Used by the download
/// status indicator so the user can see the asset growing as it streams in.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Run `gh release download` for `info` into `work`, rendering a LIVE download
/// status indicator while the asset streams in: a braille spinner, the bytes
/// pulled so far (polled off the on-disk partial), and elapsed seconds, redrawn
/// in place on a single line. When stdout isn't a TTY (e.g. `aish --update` from
/// a script or CI) we skip the animation and emit one static line instead, so
/// logs stay clean. Returns `Err` carrying gh's stderr on failure.
async fn download_asset(repo: &str, info: &UpdateInfo, work: &Path) -> Result<()> {
    use std::io::{IsTerminal, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let target = work.join(&info.asset_name);
    // Send gh's own output to a file (not a pipe) so a chatty download can never
    // wedge on a full pipe buffer while we're busy animating, and so we can still
    // surface the real error message on failure.
    let err_path = work.join(".gh-download.log");
    let err_file = std::fs::File::create(&err_path).context("creating the gh download log")?;

    let mut child = tokio::process::Command::new("gh")
        .args([
            "release",
            "download",
            &info.tag,
            "--repo",
            repo,
            "--pattern",
            &info.asset_name,
            "--dir",
            &work.to_string_lossy(),
            "--clobber",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(err_file))
        .spawn()
        .context("running `gh release download`")?;

    let interactive = std::io::stdout().is_terminal();
    if !interactive {
        // No place to animate — announce the download once and let it run.
        println!("\x1b[2mdownloading {} …\x1b[0m", info.asset_name);
    }

    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let started = Instant::now();
    let mut frame = 0usize;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("waiting on `gh release download`")?
        {
            break status;
        }
        if interactive {
            let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
            let mut out = std::io::stdout().lock();
            // \r to the line start, redraw, \x1b[K to wipe any leftover tail.
            let _ = write!(
                out,
                "\r\x1b[2m{}\x1b[0m downloading {} — \x1b[1m{}\x1b[0m in {:.0}s\x1b[K",
                FRAMES[frame],
                info.asset_name,
                human_bytes(bytes),
                started.elapsed().as_secs_f64(),
            );
            let _ = out.flush();
            frame = (frame + 1) % FRAMES.len();
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    };

    if interactive {
        // Clear the spinner line so the final state prints fresh.
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[K");
        let _ = out.flush();
    }

    if !status.success() {
        let err = std::fs::read_to_string(&err_path).unwrap_or_default();
        bail!("gh release download failed: {}", err.trim());
    }

    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    println!(
        "\x1b[32m✓\x1b[0m downloaded {} ({})",
        info.asset_name,
        human_bytes(bytes)
    );
    Ok(())
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
    writable_check(dest_dir).with_context(|| format!("{} is not writable", dest_dir.display()))?;

    // Scratch dir for the download + extraction; auto-cleaned on drop via the
    // explicit remove at the end (best-effort).
    let work = std::env::temp_dir().join(format!("aish-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("creating the update scratch dir")?;

    // Fetch the asset with a live download status indicator.
    if let Err(e) = download_asset(&repo, info, &work).await {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e);
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
            bail!(
                "tar failed: {}",
                String::from_utf8_lossy(&untar.stderr).trim()
            );
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
    std::fs::rename(&staged, &current_exe)
        .with_context(|| format!("replacing {} with the new binary", current_exe.display()))?;

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

// ---------------------------------------------------------------------------
// S9.4 — `:update --drain` (graceful shutdown + shell restart)
// ---------------------------------------------------------------------------
//
// `:update` (today's path) swaps the binary and advises a manual restart. The
// drain path adds a strict, gated shutdown so a self-update can happen mid-
// session without losing in-flight work:
//
//     quiesce (checkpoint + detach attached work, await background to a
//              turn-boundary up to AISH_DRAIN_TIMEOUT)
//        └─ (gate) ─▶ swap (update::perform — the pure atomic binary swap)
//                        └─ (gate) ─▶ restart (re-exec the new binary in place)
//
// The two gates are the headline safety invariant (AC5): the swap is reached
// ONLY after quiesce returns, and the restart is reached ONLY after a
// successful swap — a swap failure NEVER restarts (no half-updated restart).
//
// Containers (S9.1) have their own PID 1 and a detached lifecycle, so the shell
// exiting/re-execing leaves them running (AC3) — drain only checkpoints the
// interactive/attached + host-side work, it never babysits containers. Post-
// restart rediscovery of surviving container workers (AC6) is S9.5's job; this
// module ends at the re-exec.

/// The internal checkpoint signal placed on a live coordinator's
/// `coordinator_messages` mailbox (the same authenticated channel `:tell`
/// uses). A coordinator that recognises this sentinel at its round boundary
/// flushes its S9.3 transcript and, if attached, detaches — so a restart can't
/// sever it mid-turn. A fixed internal token, not user-forgeable into code.
pub const CHECKPOINT_SENTINEL: &str = "__aish_checkpoint_detach__";

/// Default bound on how long `drain` awaits background jobs reaching a turn-
/// boundary checkpoint before proceeding (AC4). Overridable per-invocation via
/// `AISH_DRAIN_TIMEOUT` (whole seconds).
pub const DEFAULT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The drain await bound, read from `AISH_DRAIN_TIMEOUT` (seconds). Falls back
/// to [`DEFAULT_DRAIN_TIMEOUT`] when the var is unset, unparseable, or zero.
pub fn drain_timeout() -> std::time::Duration {
    std::env::var("AISH_DRAIN_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_DRAIN_TIMEOUT)
}

/// Everything `drain` / `perform_with_drain` need to quiesce background work,
/// captured up front so the orchestration is decoupled from `Session`. The
/// caller (the `:update --drain` handler) fills it from the live session.
pub struct DrainCtx<'a> {
    /// The durable coordinator store — the source of truth for which runs are
    /// live (goal-loop turns, other-session runs, reattached runs). `None` when
    /// the session has no DB (the in-memory tallies are still awaited).
    pub store: Option<&'a crate::db::CoordinatorStore>,
    /// This session's in-memory re-exec'd workers (`:dispatch` / run_in_background).
    pub worker_jobs: &'a crate::worker::WorkerJobs,
    /// This session's in-memory Anthropic batch jobs.
    pub batch_jobs: &'a crate::batch::BatchJobs,
    /// The signalling session id, recorded as the message sender for provenance.
    pub session_id: &'a str,
    /// Whether background workers are containerized (S9.1). True ⇒ they survive
    /// the restart; false (host subprocesses, the default today) ⇒ they would
    /// die, which the AC8 confirmation gate covers BEFORE drain runs.
    pub backend_is_container: bool,
    /// The bounded await for background quiescence (AC4).
    pub timeout: std::time::Duration,
}

/// What `drain` observed: which attached runs were signalled to checkpoint+
/// detach, which reached a quiescent checkpoint within the timeout, and which
/// were left mid-flight at the deadline (safe once containerized + persisted;
/// recorded for S9.5 rediscovery). A transient, in-memory summary (no schema).
#[derive(Debug, Default, Clone)]
pub struct DrainReport {
    /// Run ids handed a checkpoint+detach signal (AC2).
    pub attached_detached: Vec<String>,
    /// Run ids that quiesced (became terminal) before the deadline.
    pub checkpointed: Vec<String>,
    /// Run ids still active at the deadline — proceeded past, recorded (AC4).
    pub left_mid_flight: Vec<String>,
}

/// The terminal outcome a staged drain reaches. Pure so the gate ordering
/// (AC5) is unit-testable without a real swap or re-exec.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)] // pure AC5 gate model; exercised by drain_tests + documents the invariant
pub enum StagedOutcome {
    /// Quiesce stage returned an error — swap NEVER ran.
    QuiesceFailed(String),
    /// Quiesce ok, but the binary swap failed — restart NEVER ran (AC5).
    SwapFailed(String),
    /// Swap ok, but the re-exec returned (failure) — binary already in place.
    RestartFailed(String),
    /// Swap ok and the re-exec replaced the process image (normally unreachable
    /// — `exec` doesn't return on success; modelled for completeness/tests).
    Restarted,
}

/// Drive the strict gated sequence quiesce → swap → restart. Each stage is a
/// closure returning `Result<(), String>`; the swap closure runs ONLY when
/// quiesce returns `Ok`, and restart runs ONLY when swap returns `Ok` — the
/// headline ordering invariant (AC5). Pure over its closures so the gate can be
/// asserted with stage stubs (a failing quiesce must leave swap/restart
/// untouched; a failing swap must leave restart untouched).
#[allow(dead_code)] // pure AC5 gate model; exercised by drain_tests
pub fn run_staged<Q, S, R>(quiesce: Q, swap: S, restart: R) -> StagedOutcome
where
    Q: FnOnce() -> Result<(), String>,
    S: FnOnce() -> Result<(), String>,
    R: FnOnce() -> Result<(), String>,
{
    match quiesce() {
        Err(e) => StagedOutcome::QuiesceFailed(e),
        Ok(()) => match swap() {
            Err(e) => StagedOutcome::SwapFailed(e),
            Ok(()) => match restart() {
                Err(e) => StagedOutcome::RestartFailed(e),
                Ok(()) => StagedOutcome::Restarted,
            },
        },
    }
}

/// Partition the set of signalled run ids into those that quiesced (NOT in the
/// still-active set at the deadline) and those left mid-flight (still active).
/// Pure → unit-tested for the AC4 report split.
pub fn partition_drain(
    signalled: &[String],
    still_active: &std::collections::HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let mut checkpointed = Vec::new();
    let mut left = Vec::new();
    for id in signalled {
        if still_active.contains(id) {
            left.push(id.clone());
        } else {
            checkpointed.push(id.clone());
        }
    }
    (checkpointed, left)
}

/// The AC8 host-subprocess confirmation prompt: with host workers in flight, a
/// restart WILL lose them (they're children of the shell), so the user must
/// confirm explicitly. Pure over the count for unit-testing.
pub fn host_drain_warning(running: usize) -> String {
    let plural = if running == 1 { "" } else { "s" };
    format!(
        "\u{26a0} {running} background job{plural} run as HOST subprocesses (no container backend) and WILL be terminated by the restart. Checkpoint what we can and restart anyway?"
    )
}

/// The set of live (non-terminal) coordinator run ids in the durable store,
/// per [`crate::coordinator::Phase`]. Empty on no store / read error (best-
/// effort: a store hiccup must never wedge a drain).
fn live_run_ids(ctx: &DrainCtx<'_>) -> Vec<String> {
    let Some(store) = ctx.store else {
        return Vec::new();
    };
    store
        .load_all()
        .map(|rows| {
            rows.into_iter()
                .filter(|r| {
                    matches!(
                        crate::coordinator::Phase::parse(&r.phase),
                        crate::coordinator::Phase::Coordinating
                            | crate::coordinator::Phase::AwaitingBatch
                    )
                })
                .map(|r| r.run_id)
                .collect()
        })
        .unwrap_or_default()
}

/// Total background work still tied to THIS binary: in-memory batches + workers
/// + durable coordinator runs the in-memory tallies miss. Mirrors the prompt's
/// ⟳N tally; the quiesce loop polls this to zero (or the timeout).
fn drain_running_count(ctx: &DrainCtx<'_>) -> usize {
    let batches = crate::batch::running_count(ctx.batch_jobs);
    let workers = crate::worker::running_count(ctx.worker_jobs);
    let coordinators = ctx.store.map_or(0, |store| {
        let in_memory: std::collections::HashSet<String> = ctx
            .worker_jobs
            .lock()
            .unwrap()
            .iter()
            .map(|w| w.id.clone())
            .collect();
        crate::coordinator::active_store_count(store, &in_memory)
    });
    batches + workers + coordinators
}

/// Quiesce interactive + background work (stages 1–2 of the drain, AC2/AC4):
///   1. Broadcast the [`CHECKPOINT_SENTINEL`] to every live coordinator run via
///      its `coordinator_messages` mailbox so it flushes S9.3 state and detaches.
///   2. Poll the combined background tally to zero, bounded by `ctx.timeout` at
///      a 200ms cadence. On timeout, the still-active runs are recorded as
///      `left_mid_flight` (safe once containerized + persisted) and we proceed.
/// Always returns a report — drain is best-effort; a store error degrades to an
/// empty signal set rather than failing the update.
pub async fn drain(ctx: &DrainCtx<'_>) -> DrainReport {
    let mut report = DrainReport::default();

    // Stage 1 — broadcast the checkpoint+detach signal to every live run.
    let signalled = live_run_ids(ctx);
    if let Some(store) = ctx.store {
        for run_id in &signalled {
            // Best-effort: a mailbox write failure for one run must not abort
            // the whole drain (the others still need signalling).
            let _ = store.enqueue_message(run_id, CHECKPOINT_SENTINEL, Some(ctx.session_id));
        }
    }
    report.attached_detached = signalled.clone();

    // Stage 2 — bounded await for a quiescent (turn-boundary) state. Container-
    // backed workers (S9.1) have their own PID 1 and survive the restart, so we
    // do NOT block on them (AC3/AC4) — only host-subprocess work, which would die
    // on restart, is awaited to a checkpoint up to the timeout.
    if !ctx.backend_is_container {
        let deadline = std::time::Instant::now() + ctx.timeout;
        loop {
            if drain_running_count(ctx) == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    // Partition the signalled runs by what's still active at the deadline.
    let still_active: std::collections::HashSet<String> = live_run_ids(ctx).into_iter().collect();
    let (checkpointed, left) = partition_drain(&signalled, &still_active);
    report.checkpointed = checkpointed;
    report.left_mid_flight = left;
    report
}

/// Re-exec the (freshly-swapped) current binary in place, preserving argv and
/// the controlling terminal — the cheapest restart that keeps the user's
/// session on the same TTY. Resolves `current_exe` AFTER the swap (canonicalized,
/// as `perform` does) so it picks up the NEW on-disk inode, never a stale path.
/// On success `exec` replaces the process image and NEVER returns; it returns an
/// `io::Error` ONLY when the re-exec fails (the caller then falls back to the
/// manual-restart advice — the binary is already in place, not lost).
#[cfg(unix)]
pub fn restart_in_place() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => std::fs::canonicalize(&p).unwrap_or(p),
        Err(e) => return e,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `exec` only returns on failure.
    std::process::Command::new(exe).args(args).exec()
}

/// The full staged drain-update: pre-flight → quiesce → (gate) swap → (gate)
/// restart. The `?` after [`perform`] is the AC5 gate — a swap failure returns
/// the error and the restart below is UNREACHABLE (no half-updated restart).
/// `drain` is best-effort and always returns a report, so the only `Err` this
/// produces is a pre-flight (`writable_check`) or swap failure. On a successful
/// swap it calls [`restart_in_place`], which returns ONLY if the re-exec failed
/// — surfaced as `restart_error` so the caller can advise a manual restart with
/// the new binary already in place.
pub async fn perform_with_drain(info: &UpdateInfo, ctx: &DrainCtx<'_>) -> Result<DrainOutcome> {
    // Pre-flight: fail fast if the destination isn't writable, BEFORE quiescing
    // for nothing (edge case — don't tear down interactive work then bail).
    let current_exe = std::env::current_exe().context("locating the running aish binary")?;
    let current_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
    let dest_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("can't determine the directory of {}", current_exe.display()))?;
    writable_check(dest_dir)
        .with_context(|| format!("{} is not writable", dest_dir.display()))?;

    // Stage 1 — quiesce (always returns a report).
    let report = drain(ctx).await;

    // Stage 2 — swap. The `?` is the AC5 gate: on ANY swap error we return here
    // and the restart below never runs.
    perform(info).await?;

    // Stage 3 — restart (reached ONLY after a successful swap). `restart_in_place`
    // returns only on re-exec failure.
    let restart_error = Some(restart_in_place());
    Ok(DrainOutcome { report, restart_error })
}

/// The result of a [`perform_with_drain`] that got past the swap. `restart_error`
/// is `Some` only when the post-swap re-exec FAILED (the new binary is in place;
/// advise a manual restart). On a successful re-exec the process image is
/// replaced and this value is never constructed.
pub struct DrainOutcome {
    pub report: DrainReport,
    pub restart_error: Option<std::io::Error>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- pure env-var parse helpers (TASK-253 / FR-305) -------------------

    #[test]
    fn parse_check_ttl_defaults_and_overrides() {
        // Unset / unparseable / empty → default 24h.
        assert_eq!(parse_check_ttl(None), DEFAULT_UPDATE_CHECK_TTL_SECS);
        assert_eq!(parse_check_ttl(Some("nope")), DEFAULT_UPDATE_CHECK_TTL_SECS);
        assert_eq!(parse_check_ttl(Some("")), DEFAULT_UPDATE_CHECK_TTL_SECS);
        // Whole seconds honoured; 0 means "always fetch fresh".
        assert_eq!(parse_check_ttl(Some("0")), 0);
        assert_eq!(parse_check_ttl(Some("3600")), 3600);
        assert_eq!(parse_check_ttl(Some("  120  ")), 120);
    }

    #[test]
    fn parse_cache_path_some_when_set_else_none() {
        use std::ffi::OsStr;
        // Unset → None (caller supplies the default location).
        assert_eq!(parse_cache_path(None), None);
        // Empty string → None (treated as unset; never an empty path).
        assert_eq!(parse_cache_path(Some(OsStr::new(""))), None);
        // Non-empty → Some(path) verbatim.
        assert_eq!(
            parse_cache_path(Some(OsStr::new("/tmp/uc.json"))),
            Some(PathBuf::from("/tmp/uc.json"))
        );
    }

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
    fn channel_parses_case_insensitively_with_default() {
        assert_eq!(parse_channel("prod"), Some(Channel::Prod));
        assert_eq!(parse_channel("STABLE"), Some(Channel::Prod));
        assert_eq!(parse_channel(" Dev "), Some(Channel::Dev));
        assert_eq!(parse_channel("nightly"), Some(Channel::Dev));
        assert_eq!(parse_channel("CI"), Some(Channel::Ci));
        assert_eq!(parse_channel("bogus"), None);
        assert_eq!(parse_channel(""), None);
    }

    #[test]
    fn channel_tag_prefixes() {
        assert_eq!(Channel::Prod.tag_prefix(), None);
        assert_eq!(Channel::Dev.tag_prefix(), Some("dev-"));
        assert_eq!(Channel::Ci.tag_prefix(), Some("ci-"));
    }

    #[test]
    fn first_with_prefix_picks_newest_match() {
        // gh release list returns newest-first; first match wins.
        let tags = vec![
            "ci-42-abc12345".to_string(),
            "dev-v0.24.0-dev.3".to_string(),
            "dev-v0.24.0-dev.2".to_string(),
            "v0.23.0".to_string(),
        ];
        assert_eq!(first_with_prefix(&tags, "dev-"), Some("dev-v0.24.0-dev.3"));
        assert_eq!(first_with_prefix(&tags, "ci-"), Some("ci-42-abc12345"));
        assert_eq!(first_with_prefix(&tags, "nope-"), None);
        assert_eq!(first_with_prefix(&[], "dev-"), None);
    }

    #[test]
    fn dev_ci_tags_trigger_update_via_string_fallback() {
        // Non-semver dev/ci tags fall back to string inequality in is_newer.
        assert!(is_newer("dev-v0.24.0-dev.3", "0.23.0"));
        assert!(is_newer("ci-42-abc12345", "0.23.0"));
    }

    #[test]
    fn matches_asset_for_known_triples() {
        let assets = vec![
            GhAsset {
                name: "aish-v0.3.0-x86_64-unknown-linux-gnu.tar.gz".into(),
            },
            GhAsset {
                name: "aish-v0.3.0-aarch64-apple-darwin.tar.gz".into(),
            },
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
            GhAsset {
                name: "aish-x86_64-unknown-linux-gnu".into(),
            },
            GhAsset {
                name: "aish-x86_64-unknown-linux-gnu.sha256".into(),
            },
            GhAsset {
                name: "aish-aarch64-apple-darwin".into(),
            },
            GhAsset {
                name: "aish-aarch64-apple-darwin.sha256".into(),
            },
            GhAsset {
                name: "aish-x86_64-apple-darwin".into(),
            },
            GhAsset {
                name: "aish-x86_64-apple-darwin.sha256".into(),
            },
            GhAsset {
                name: "SHA256SUMS".into(),
            },
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
        let assets = vec![GhAsset {
            name: "aish-v0.3.0-sparc64-unknown-haiku.tar.gz".into(),
        }];
        assert!(match_asset(&assets).is_none());
    }

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(25 * 1024 * 1024 + 700 * 1024), "25.7 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn current_version_is_valid_semver() {
        // Safeguard: ensure Cargo.toml version is always a valid semver that the
        // version detection logic can parse. This test fails if someone forgets
        // to update Cargo.toml when cutting a release (the bug from ISS-2XXX).
        let v = current_version();
        assert!(
            parse_semver(v).is_some(),
            "Cargo.toml version '{}' is not valid semver — ensure it matches the release tag",
            v
        );
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn run_staged_swap_unreachable_unless_quiesce_ok() {
        // A failing quiesce must short-circuit: swap and restart never run.
        let swap_ran = std::cell::Cell::new(false);
        let restart_ran = std::cell::Cell::new(false);
        let out = run_staged(
            || Err("nope".to_string()),
            || {
                swap_ran.set(true);
                Ok(())
            },
            || {
                restart_ran.set(true);
                Ok(())
            },
        );
        assert_eq!(out, StagedOutcome::QuiesceFailed("nope".into()));
        assert!(!swap_ran.get(), "swap must not run when quiesce fails (AC5)");
        assert!(!restart_ran.get(), "restart must not run when quiesce fails");
    }

    #[test]
    fn run_staged_restart_unreachable_unless_swap_ok() {
        // Quiesce ok, swap fails → restart NEVER runs (the no-half-update gate).
        let restart_ran = std::cell::Cell::new(false);
        let out = run_staged(
            || Ok(()),
            || Err("swap blew up".to_string()),
            || {
                restart_ran.set(true);
                Ok(())
            },
        );
        assert_eq!(out, StagedOutcome::SwapFailed("swap blew up".into()));
        assert!(!restart_ran.get(), "restart must not run when swap fails (AC5)");
    }

    #[test]
    fn run_staged_reaches_restart_only_after_both_gates() {
        // A re-exec that "returns" (failure) surfaces as RestartFailed; both
        // earlier stages must have passed to get here.
        let out = run_staged(|| Ok(()), || Ok(()), || Err("exec returned".to_string()));
        assert_eq!(out, StagedOutcome::RestartFailed("exec returned".into()));
        // The (normally-unreachable) success path is modelled too.
        let ok = run_staged(|| Ok(()), || Ok(()), || Ok(()));
        assert_eq!(ok, StagedOutcome::Restarted);
    }

    #[test]
    fn partition_drain_splits_checkpointed_from_mid_flight() {
        let signalled = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // b is still active at the deadline; a and c quiesced.
        let mut active = HashSet::new();
        active.insert("b".to_string());
        let (checkpointed, left) = partition_drain(&signalled, &active);
        assert_eq!(checkpointed, vec!["a".to_string(), "c".to_string()]);
        assert_eq!(left, vec!["b".to_string()]);

        // Nothing active → everything checkpointed, nothing left.
        let (ck, lf) = partition_drain(&signalled, &HashSet::new());
        assert_eq!(ck, signalled);
        assert!(lf.is_empty());
    }

    #[test]
    fn drain_timeout_reads_env_with_sane_fallback() {
        // Unset / zero / garbage all fall back to the 120s default; a positive
        // integer is honoured. Serialise via the process env (test owns the var).
        // SAFETY: single-threaded test mutation of a process env var.
        unsafe { std::env::remove_var("AISH_DRAIN_TIMEOUT") };
        assert_eq!(drain_timeout(), DEFAULT_DRAIN_TIMEOUT);
        unsafe { std::env::set_var("AISH_DRAIN_TIMEOUT", "0") };
        assert_eq!(drain_timeout(), DEFAULT_DRAIN_TIMEOUT);
        unsafe { std::env::set_var("AISH_DRAIN_TIMEOUT", "notanumber") };
        assert_eq!(drain_timeout(), DEFAULT_DRAIN_TIMEOUT);
        unsafe { std::env::set_var("AISH_DRAIN_TIMEOUT", "45") };
        assert_eq!(drain_timeout(), std::time::Duration::from_secs(45));
        unsafe { std::env::remove_var("AISH_DRAIN_TIMEOUT") };
    }

    #[test]
    fn checkpoint_sentinel_is_a_fixed_internal_token() {
        // The signal coordinators recognise at their round boundary. Pinned so a
        // rename here and in coordinator::drive can't silently drift apart.
        assert_eq!(CHECKPOINT_SENTINEL, "__aish_checkpoint_detach__");
    }

    #[test]
    fn host_drain_warning_names_count_and_consequence() {
        let one = host_drain_warning(1);
        assert!(one.contains('1'));
        assert!(one.contains("HOST"));
        assert!(one.contains("terminated"));
        assert!(one.contains("job ")); // singular
        let many = host_drain_warning(3);
        assert!(many.contains('3'));
        assert!(many.contains("jobs ")); // plural
    }

    // -----------------------------------------------------------------------
    // TASK-248 — update-check cache
    // -----------------------------------------------------------------------

    fn sample_cache() -> CachedCheck {
        CachedCheck {
            last_check_ts: 1_000,
            latest_tag: "v0.30.0".into(),
            latest_version: "0.30.0".into(),
            asset_name: Some("aish-x86_64-unknown-linux-gnu".into()),
            channel: "prod".into(),
        }
    }

    #[test]
    fn cache_freshness_is_pure_over_mocked_time() {
        let ttl = 24 * 60 * 60; // 86400
        // Written at t=1000; still fresh a few hours later.
        assert!(is_cache_fresh(1_000 + 3_600, 1_000, ttl));
        // Exactly at the boundary → stale (strictly-less-than window).
        assert!(!is_cache_fresh(1_000 + ttl, 1_000, ttl));
        // Well past the window → stale.
        assert!(!is_cache_fresh(1_000 + ttl + 1, 1_000, ttl));
        // Clock ran backwards (now < last_check) → treated as stale, no panic.
        assert!(!is_cache_fresh(500, 1_000, ttl));
    }

    #[test]
    fn ttl_zero_is_never_fresh() {
        // AC3: TTL=0 forces a fresh network check regardless of timestamps.
        assert!(!is_cache_fresh(1_000, 1_000, 0));
        assert!(!is_cache_fresh(0, 0, 0));
    }

    #[test]
    fn cached_to_info_flags_newer_and_goes_quiet_when_current() {
        let c = sample_cache();
        // Current build older than cached latest → surface the upgrade.
        let info = cached_to_info(&c, "0.29.0").expect("upgrade available");
        assert_eq!(info.tag, "v0.30.0");
        assert_eq!(info.version, "0.30.0");
        assert_eq!(info.asset_name, "aish-x86_64-unknown-linux-gnu");
        // Current build == cached latest → no notice (user already upgraded).
        assert!(cached_to_info(&c, "0.30.0").is_none());
        // Current build newer than cached latest → no downgrade notice.
        assert!(cached_to_info(&c, "0.31.0").is_none());
    }

    #[test]
    fn cached_to_info_none_when_no_platform_asset() {
        // A latest release with nothing installable for this platform must not
        // produce a phantom upgrade, even when strictly newer.
        let mut c = sample_cache();
        c.asset_name = None;
        assert!(cached_to_info(&c, "0.1.0").is_none());
    }

    #[test]
    fn write_then_read_cache_roundtrips_via_filesystem() {
        let dir = std::env::temp_dir().join(format!("aish-cache-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("update-check.json");
        let original = sample_cache();
        write_cache(&path, &original).expect("write cache");
        let read = read_cache(&path).expect("read back cache");
        assert_eq!(read.last_check_ts, original.last_check_ts);
        assert_eq!(read.latest_tag, original.latest_tag);
        assert_eq!(read.latest_version, original.latest_version);
        assert_eq!(read.asset_name, original.asset_name);
        assert_eq!(read.channel, original.channel);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_cache_missing_or_corrupt_is_none() {
        // AC2: a missing file reads as None (→ network fallback).
        let missing = std::env::temp_dir().join("aish-cache-does-not-exist-xyz.json");
        let _ = std::fs::remove_file(&missing);
        assert!(read_cache(&missing).is_none());
        // A corrupt file also reads as None rather than erroring.
        let dir = std::env::temp_dir().join(format!("aish-cache-corrupt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("update-check.json");
        std::fs::write(&path, b"{ not valid json ]").expect("write corrupt");
        assert!(read_cache(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
