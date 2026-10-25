//! Version PROVENANCE formatting — what this binary actually is, derived from
//! the release tag baked in at build time (`AISH_RELEASE_TAG`, set by the
//! release workflows) plus the Cargo package version.
//!
//! Previously the statusline printed a bare `aish v0.71.2` and a dev snapshot
//! said `0.71.2 (dev snapshot dev-v0.71.2-dev.6)` only in `--version` — so the
//! one surface an operator looks at all day was indistinguishable between a
//! published release and a six-commits-later dev build. Two bugs came from
//! that: "I'm on 0.71.2" claims that were really dev snapshots, and reinstalls
//! chasing a fix that was already in the running binary.
//!
//! This module is the single source of truth for the three facts, and it is
//! PURE (tag in, strings out) so every shape is unit-testable without a build:
//!
//! | Baked tag | Channel | Rendered version |
//! |---|---|---|
//! | `` (empty — local `cargo build`) | `local` | `0.71.2` |
//! | `v0.71.2` | `prod` | `0.71.2` |
//! | `dev-v0.71.2-dev.6` | `dev` | `0.71.2+dev.6` |
//! | `ci-1234567890-abc1234` | `ci` | `0.71.2+ci.1234567890.abc1234` |
//!
//! The dev form is deliberate semver build metadata (`+dev.6`) rather than the
//! old prose: `v0.71.2+dev.6` reads as "0.71.2 plus 6 dev commits" and can
//! never be mistaken for the `v0.71.2` release, while still sorting and
//! comparing as 0.71.2 for any tool that parses semver.

/// Which lane produced this binary — a property of the BUILD, not of whatever
/// channel the operator currently tracks for updates (`:update` can differ,
/// and `:version` prints both so a mismatch is visible instead of inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    /// Built from a `v*` release tag — a published release.
    Prod,
    /// Built from a `dev-v*` snapshot tag.
    Dev,
    /// Built from a `ci-*` tag (per-PR / per-commit artifact).
    Ci,
    /// No tag baked in: someone's `cargo build` (or an unrecognised tag shape).
    Local,
}

impl BuildChannel {
    /// Short lowercase label — what the statusline chip and `:version` print.
    pub fn label(self) -> &'static str {
        match self {
            BuildChannel::Prod => "prod",
            BuildChannel::Dev => "dev",
            BuildChannel::Ci => "ci",
            BuildChannel::Local => "local",
        }
    }

    /// Whether this channel earns a statusline chip. `prod` does NOT: the
    /// release lane is the default expectation, so chipping it would add a
    /// permanent token to every bar for zero information. Any OTHER lane is
    /// the surprising case and gets the badge.
    pub fn is_noteworthy(self) -> bool {
        !matches!(self, BuildChannel::Prod)
    }
}

/// Classify the baked release tag into its build lane.
pub fn build_channel(tag: &str) -> BuildChannel {
    let tag = tag.trim();
    if tag.is_empty() {
        return BuildChannel::Local;
    }
    if tag.starts_with("dev-") {
        BuildChannel::Dev
    } else if tag.starts_with("ci-") {
        BuildChannel::Ci
    } else if tag.starts_with('v') && tag[1..].starts_with(|c: char| c.is_ascii_digit()) {
        BuildChannel::Prod
    } else {
        BuildChannel::Local
    }
}

/// Keep only characters legal in a semver build-metadata identifier
/// (`[0-9A-Za-z-]`, dot-separated); anything else becomes `-` so a hand-set
/// `AISH_RELEASE_TAG` can never emit an unparseable version string.
fn sanitize_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out
}

/// Render the user-visible version from the package version + baked tag.
///
/// `base` is `CARGO_PKG_VERSION`; `tag` is the baked `AISH_RELEASE_TAG`. A
/// release tag wins over `base` when they disagree (the tag is what was
/// actually published), so a stale `Cargo.toml` can't misreport a release.
pub fn format_version(base: &str, tag: &str) -> String {
    let tag = tag.trim();
    match build_channel(tag) {
        BuildChannel::Local => base.to_string(),
        BuildChannel::Prod => tag.trim_start_matches('v').to_string(),
        BuildChannel::Dev => {
            // `dev-v0.71.2-dev.6` → version `0.71.2`, metadata `dev.6`.
            let rest = tag.trim_start_matches("dev-").trim_start_matches('v');
            match rest.split_once("-dev.") {
                Some((ver, n)) if !ver.is_empty() && !n.is_empty() => {
                    format!("{ver}+dev.{}", sanitize_meta(n))
                }
                // Unexpected dev shape: still mark it as dev rather than
                // silently rendering it as the release version.
                _ => format!("{base}+dev"),
            }
        }
        BuildChannel::Ci => {
            // `ci-1234567890-abc1234` → `0.71.2+ci.1234567890.abc1234`.
            let rest = tag.trim_start_matches("ci-");
            if rest.is_empty() {
                format!("{base}+ci")
            } else {
                format!("{base}+ci.{}", sanitize_meta(&rest.replace('-', ".")))
            }
        }
    }
}

// ── runtime layer ──────────────────────────────────────────────────────────
// Everything above is pure. Below, the baked-in facts are read ONCE and the
// three provenance questions get one answer each: what version, which lane,
// which binary.
//
// NOTE: declared after the pure helpers and before the tests.

/// The release tag baked in at build time by `build.rs` from the
/// `AISH_RELEASE_TAG` env var. Empty for a plain local `cargo build`.
pub const BAKED_TAG: &str = env!("AISH_RELEASE_TAG");

/// The lane that produced THIS binary (see [`BuildChannel`]).
pub fn channel() -> BuildChannel {
    build_channel(BAKED_TAG)
}

/// The user-visible version — `0.71.2`, `0.71.2+dev.6`, `0.71.2+ci.…`.
///
/// Computed once and handed out as `&'static str` so the long-standing
/// `&'static str` call sites (e.g. `crate::update::current_version`) keep
/// working without a signature change.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| format_version(env!("CARGO_PKG_VERSION"), BAKED_TAG))
        .as_str()
}

/// Pure form of the statusline badge text: the version plus a channel chip for
/// any lane that isn't `prod`. Prod stays bare so the bar gains a token only
/// when the build is actually something other than a release.
pub fn format_statusline_version(version: &str, ch: BuildChannel) -> String {
    if ch.is_noteworthy() {
        format!("{version} ({})", ch.label())
    } else {
        version.to_string()
    }
}

/// What the statusline prints: [`version`] with the channel chip applied.
pub fn statusline_version() -> &'static str {
    static BADGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BADGE
        .get_or_init(|| format_statusline_version(version(), channel()))
        .as_str()
}

/// Pure form of the `:version` report. Takes every fact as an argument so the
/// exact shape is testable with no build, no TTY, and no filesystem.
///
/// `update_channel` is what the operator currently TRACKS for updates, which
/// can differ from the lane that built the running binary — printing both is
/// the point: a `dev` build tracking `prod` is a real and previously invisible
/// state.
pub fn format_report(
    version: &str,
    build: BuildChannel,
    update_channel: &str,
    tag: &str,
    exe: &str,
) -> String {
    let channel_line = if build.label() == update_channel {
        format!("{} (build + updates)", build.label())
    } else {
        format!("{} (build) · {} (updates)", build.label(), update_channel)
    };
    let tag = if tag.trim().is_empty() {
        "none (local build)"
    } else {
        tag.trim()
    };
    format!("aish v{version}\n  channel    {channel_line}\n  build tag  {tag}\n  binary     {exe}")
}

/// The `:version` report for this process: version, channel(s), baked tag, and
/// the RESOLVED path of the running executable — the fact that settles "which
/// aish am I actually running?" when several are installed.
pub fn report() -> String {
    // Canonicalize: installs are routinely a symlink on PATH
    // (`~/.local/bin/aish` → `~/.aish/bin/aish-0.71.2`), and the whole point of
    // printing the path is to name the FILE that is running — a symlink would
    // leave the original question open. Fall back to the raw path if the
    // canonical form is unavailable (deleted/replaced binary mid-run).
    let exe = std::env::current_exe()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p).display().to_string())
        .unwrap_or_else(|e| format!("<unresolved: {e}>"));
    format_report(
        version(),
        channel(),
        crate::update::channel().as_str(),
        BAKED_TAG,
        &exe,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_build_renders_bare_package_version() {
        assert_eq!(build_channel(""), BuildChannel::Local);
        assert_eq!(build_channel("   "), BuildChannel::Local);
        assert_eq!(format_version("0.71.2", ""), "0.71.2");
        // An unrecognised tag shape must NOT be dressed up as a release.
        assert_eq!(build_channel("nightly-whatever"), BuildChannel::Local);
        assert_eq!(format_version("0.71.2", "nightly-whatever"), "0.71.2");
        // `v` with no digit after it is not a release tag either.
        assert_eq!(build_channel("vnext"), BuildChannel::Local);
    }

    #[test]
    fn release_tag_is_authoritative_over_cargo_version() {
        assert_eq!(build_channel("v0.71.2"), BuildChannel::Prod);
        assert_eq!(format_version("0.71.2", "v0.71.2"), "0.71.2");
        // Stale Cargo.toml vs the tag that actually shipped: trust the tag.
        assert_eq!(format_version("0.70.0", "v0.71.2"), "0.71.2");
        // prod is the unremarkable default — no statusline chip.
        assert!(!BuildChannel::Prod.is_noteworthy());
    }

    #[test]
    fn dev_snapshot_renders_as_semver_build_metadata() {
        assert_eq!(build_channel("dev-v0.71.2-dev.6"), BuildChannel::Dev);
        // The headline requirement: v0.71.2+dev.6, never plain v0.71.2.
        assert_eq!(
            format_version("0.71.2", "dev-v0.71.2-dev.6"),
            "0.71.2+dev.6"
        );
        assert_ne!(format_version("0.71.2", "dev-v0.71.2-dev.6"), "0.71.2");
        // Tag version wins here too.
        assert_eq!(
            format_version("0.71.1", "dev-v0.71.2-dev.6"),
            "0.71.2+dev.6"
        );
        // Malformed dev tag still reads as a dev build, not a release.
        assert_eq!(format_version("0.71.2", "dev-garbage"), "0.71.2+dev");
        assert!(BuildChannel::Dev.is_noteworthy());
    }

    #[test]
    fn ci_artifact_is_labelled_and_sanitized() {
        assert_eq!(build_channel("ci-1234567890-abc1234"), BuildChannel::Ci);
        assert_eq!(
            format_version("0.71.2", "ci-1234567890-abc1234"),
            "0.71.2+ci.1234567890.abc1234"
        );
        assert_eq!(format_version("0.71.2", "ci-"), "0.71.2+ci");
        // Illegal build-metadata characters degrade to `-`, never leak through.
        assert_eq!(
            format_version("0.71.2", "ci-feat/thing_1"),
            "0.71.2+ci.feat-thing-1"
        );
        assert!(BuildChannel::Ci.is_noteworthy());
    }

    #[test]
    fn channel_labels_are_stable_lowercase_tokens() {
        assert_eq!(BuildChannel::Prod.label(), "prod");
        assert_eq!(BuildChannel::Dev.label(), "dev");
        assert_eq!(BuildChannel::Ci.label(), "ci");
        assert_eq!(BuildChannel::Local.label(), "local");
    }

    #[test]
    fn statusline_chips_only_non_prod_builds() {
        // A release build must not gain a chip — the bar stays as it is today.
        assert_eq!(
            format_statusline_version("0.71.2", BuildChannel::Prod),
            "0.71.2"
        );
        // Everything else announces itself.
        assert_eq!(
            format_statusline_version("0.71.2+dev.6", BuildChannel::Dev),
            "0.71.2+dev.6 (dev)"
        );
        assert_eq!(
            format_statusline_version("0.71.2+ci.1.a", BuildChannel::Ci),
            "0.71.2+ci.1.a (ci)"
        );
        assert_eq!(
            format_statusline_version("0.71.2", BuildChannel::Local),
            "0.71.2 (local)"
        );
    }

    #[test]
    fn report_shows_version_both_channels_tag_and_binary() {
        let r = format_report(
            "0.71.2+dev.6",
            BuildChannel::Dev,
            "prod",
            "dev-v0.71.2-dev.6",
            "/usr/local/bin/aish",
        );
        assert!(r.starts_with("aish v0.71.2+dev.6\n"), "{r}");
        // The previously invisible state: a dev binary tracking prod updates.
        assert!(r.contains("dev (build) · prod (updates)"), "{r}");
        assert!(r.contains("dev-v0.71.2-dev.6"), "{r}");
        // The fact that answers "which aish am I running?".
        assert!(r.contains("/usr/local/bin/aish"), "{r}");
    }

    #[test]
    fn report_collapses_matching_channels_and_names_local_builds() {
        let r = format_report("0.71.2", BuildChannel::Prod, "prod", "v0.71.2", "/opt/aish");
        assert!(r.contains("prod (build + updates)"), "{r}");
        assert!(!r.contains("(updates)\n  build tag"), "{r}");

        // No tag baked in ⇒ say so plainly rather than printing an empty field.
        let local = format_report("0.71.2", BuildChannel::Local, "prod", "   ", "/tmp/aish");
        assert!(local.contains("none (local build)"), "{local}");
        assert!(local.contains("local (build) · prod (updates)"), "{local}");
    }
}
