//! Sensitive-path denylist — SEC-2.3 / F-09 (TASK-945).
//!
//! Path gating elsewhere in aish is *uniform*: every path is equally grantable,
//! and the `'d'` answer in [`crate::tools`]'s `gate_path` grants **every** access
//! of that permission under a directory, recursively. One `'d'` on `~` would
//! permanently authorise `~/.ssh/authorized_keys`. `Yolo` mode skips path gating
//! altogether.
//!
//! Some paths must never be silently reachable, regardless of mode or prior
//! grant. This module is the single source of truth for which ones:
//!
//! * [`classify_path`] resolves a path (symlinks, `..`, non-existent tails) and
//!   classifies it [`Sensitivity::Benign`] or [`Sensitivity::Sensitive`].
//! * [`path_gate`] turns that classification into a [`Gate`] decision: an
//!   unattended/coordinator run **refuses**, every interactive mode — *including
//!   `Yolo`* — **confirms**. Sensitive paths are the single documented exception
//!   to Yolo's "confirm nothing" contract.
//! * [`grants_apply`] is the ordering rule: classify FIRST, consult grants
//!   SECOND. A pre-existing `'d'`/`'a'` grant never covers a sensitive path.
//! * [`argv_sensitivity`] scans a `run_program` argv, because `cat
//!   ~/.ssh/id_rsa` is a benign *program* with a breaching *argument*.
//!
//! Fail-closed: any path that cannot be resolved is treated as sensitive.

use crate::session::Mode;
use std::path::{Component, Path, PathBuf};

/// Directory (and single-file) patterns holding credentials, keys, or identity
/// material. Each entry is a `/`-separated run of path components matched
/// against any window of the CANONICALISED path, so `.config/gh` matches
/// `~/.config/gh/hosts.yml` and `/opt/home/.config/gh/hosts.yml` alike.
///
/// NOTE (risk table): `.aish` and `.atum` are deliberately **narrowed to the
/// credential files** rather than blanket-listed. aish keeps its session DB,
/// MCP config, worktrees, and skills under `~/.aish` — denylisting the whole
/// directory would make aish prompt on its own bookkeeping writes and break the
/// REPL. See the `aish_own_config_benign` test.
pub static SENSITIVE_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".config/gh",
    ".config/gcloud",
    ".aish/credentials",
    ".atum/credentials",
    ".netrc",
    ".password-store",
    ".gem/credentials",
    ".azure",
    ".terraform.d",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
];

/// File-name globs (only `*` is a wildcard) matched against the final component
/// of the canonicalised path.
pub static SENSITIVE_FILE_GLOBS: &[&str] = &[
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "id_rsa*",
    "id_dsa*",
    "id_ecdsa*",
    "id_ed25519*",
    ".env",
    ".env.*",
    "*.env",
    "credentials",
    "kubeconfig",
    ".netrc",
    ".htpasswd",
    "*.ovpn",
    "*.kdbx",
    "service-account*.json",
];

/// Absolute system paths. Matched component-wise against both the lexical and
/// the canonical form of the path, so `/etc/sshd_config` does **not** match
/// `/etc/ssh`.
pub static SENSITIVE_ABS: &[&str] = &[
    "/etc/shadow",
    "/etc/passwd",
    "/etc/sudoers",
    "/etc/ssh",
    "/root",
    "/var/run/docker.sock",
    "/proc/self/environ",
];

/// Names that look like `*.env` / `.env.*` but carry no secrets — committed
/// templates. Checked BEFORE [`SENSITIVE_FILE_GLOBS`] (but after the directory
/// rules: a template inside `~/.ssh` is still sensitive).
pub static SENSITIVE_EXEMPT: &[&str] = &[
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    "*.env.example",
    "*.env.sample",
    "*.env.template",
    "*.env.dist",
];

/// Whether a path holds credential/key/identity material.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sensitivity {
    Benign,
    Sensitive {
        /// The denylist entry that matched — shown in the prompt so the user can
        /// see *why* aish is asking.
        reason: &'static str,
    },
}

impl Sensitivity {
    pub fn is_sensitive(self) -> bool {
        matches!(self, Sensitivity::Sensitive { .. })
    }

    pub fn reason(self) -> Option<&'static str> {
        match self {
            Sensitivity::Sensitive { reason } => Some(reason),
            Sensitivity::Benign => None,
        }
    }
}

/// What the safety gate must do with a path before the normal per-mode logic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gate {
    /// Not sensitive — fall through to the existing per-mode gating.
    Normal,
    /// Sensitive: confirm, in EVERY mode including `Yolo`.
    Confirm { reason: &'static str },
    /// Sensitive and nobody is at the keyboard: refuse outright.
    Refuse { reason: &'static str },
}

/// Marker prefix on a confirmation prompt that flags it as a sensitive-path
/// prompt. The frontend strips it, renders only `y/N` (no `'a'`/`'d'`), and
/// clamps the answer via [`clamp_decision`]. A control character keeps it out of
/// any legitimate prompt text.
pub const SENSITIVE_MARK: &str = "\u{1}";

/// Build the confirmation prompt for a sensitive path: the marker, a line
/// naming the matched denylist entry, then the caller's normal prompt body.
pub fn sensitive_prompt(reason: &str, body: &str) -> String {
    format!("{SENSITIVE_MARK}sensitive path ({reason}) — {body}")
}

/// Split a prompt into `(display body, is_sensitive)`.
pub fn split_prompt(prompt: &str) -> (&str, bool) {
    match prompt.strip_prefix(SENSITIVE_MARK) {
        Some(rest) => (rest, true),
        None => (prompt, false),
    }
}

/// The option list a prompt may offer. A sensitive path can never be
/// always-allowed or directory-granted, and an option that cannot be honoured
/// must not be displayed.
pub fn prompt_options(sensitive: bool) -> &'static str {
    if sensitive { "[y/N]" } else { "[y/N/a/d]" }
}

/// Clamp a parsed answer for a sensitive prompt: only a one-shot `y` is
/// honourable, so `'a'` (always-allow) and `'d'` (directory grant) degrade to a
/// refusal rather than silently persisting an unhonourable grant.
pub fn clamp_decision(sensitive: bool, d: crate::tools::Decision) -> crate::tools::Decision {
    use crate::tools::Decision;
    if !sensitive {
        return d;
    }
    match d {
        Decision::AllowOnce => Decision::AllowOnce,
        Decision::Deny | Decision::AlwaysAllow | Decision::AllowDir => Decision::Deny,
    }
}

/// Classify a path. The match runs against the CANONICALISED path so
/// `~/x/../.ssh/id_rsa`, `~/./.ssh/id_rsa`, and a symlink pointing into
/// `~/.ssh` all classify alike.
///
/// Fails closed: when the path cannot be resolved at all, it is Sensitive.
pub fn classify_path(p: &Path) -> Sensitivity {
    let lexical = match absolutise(p) {
        Some(a) => normalise_lexically(&a),
        // No cwd, no home, nothing to anchor a relative path to — fail closed.
        None => {
            return Sensitivity::Sensitive {
                reason: "unresolvable path",
            };
        }
    };
    let Some(canonical) = resolve_for_match(&lexical) else {
        return Sensitivity::Sensitive {
            reason: "unresolvable path (fail closed)",
        };
    };

    // Absolute system paths: check both forms. `/proc/self/environ` canonicalises
    // to `/proc/<pid>/environ`, so the lexical form is the one that matches it.
    for form in [&lexical, &canonical] {
        if let Some(reason) = match_abs(form) {
            return Sensitivity::Sensitive { reason };
        }
    }

    let comps = normal_components(&canonical);
    if let Some(reason) = match_dirs(&comps) {
        return Sensitivity::Sensitive { reason };
    }
    // A lexical-form directory check too: a path that canonicalises *out* of a
    // sensitive directory still went in through one.
    if let Some(reason) = match_dirs(&normal_components(&lexical)) {
        return Sensitivity::Sensitive { reason };
    }

    if let Some(name) = comps.last() {
        if SENSITIVE_EXEMPT.iter().any(|e| glob_match(e, name)) {
            return Sensitivity::Benign;
        }
        if let Some(pat) = SENSITIVE_FILE_GLOBS.iter().find(|g| glob_match(g, name)) {
            return Sensitivity::Sensitive { reason: pat };
        }
    }
    Sensitivity::Benign
}

/// Process-wide "nobody is at the keyboard" flag. A background coordinator /
/// worker runs with an auto-allow confirm hook, so a prompt there is not a
/// question — it is a silent yes. Such a process marks itself unattended at
/// start-up ([`set_unattended`]) and sensitive paths are then REFUSED outright
/// instead of confirmed. Process-global is the right scope: a coordinator
/// process is unattended in its entirety.
static UNATTENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process unattended (no human can answer a prompt).
pub fn set_unattended(v: bool) {
    UNATTENDED.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process is unattended. `AISH_UNATTENDED=1` forces it on for
/// scripted/CI invocations that never see a TTY.
pub fn is_unattended() -> bool {
    UNATTENDED.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("AISH_UNATTENDED").is_ok_and(|v| v == "1" || v == "true")
}

/// The mode/attendance policy for a path. `Yolo` is NOT a bypass here.
pub fn path_gate(mode: Mode, unattended: bool, p: &Path) -> Gate {
    match classify_path(p) {
        Sensitivity::Benign => Gate::Normal,
        Sensitivity::Sensitive { reason } => {
            if unattended {
                // No human to answer the prompt: refuse rather than proceed.
                Gate::Refuse { reason }
            } else {
                match mode {
                    // The single documented exception to Yolo's contract.
                    Mode::Yolo | Mode::Normal | Mode::Careful | Mode::Paranoid => {
                        Gate::Confirm { reason }
                    }
                }
            }
        }
    }
}

/// Whether persisted grants (`'a'` always-allow, `'d'` directory grant) may be
/// honoured for `path`. Sensitive paths ignore every pre-existing grant —
/// classify FIRST, consult grants SECOND.
pub fn grants_apply(path: &Path) -> bool {
    !classify_path(path).is_sensitive()
}

/// Scan a `run_program` invocation: if the program itself or ANY argument
/// resolves to a sensitive path, the exec is sensitive. This is what catches
/// `cat ~/.ssh/id_rsa` and `tar -cf - ~/.aws`, where the program is benign and
/// the argument is the breach.
pub fn argv_sensitivity(cwd: &Path, program: &str, args: &[String]) -> Option<&'static str> {
    let mut candidates: Vec<&str> = Vec::with_capacity(args.len() * 2 + 1);
    // The program itself only counts when it names a path (`./x`, `/usr/bin/x`),
    // never a bare binary name resolved via PATH.
    if program.contains('/') || program.starts_with('~') {
        candidates.push(program);
    }
    for a in args {
        if a.is_empty() {
            continue;
        }
        if a.starts_with('-') {
            // `--file=~/.ssh/id_rsa` hides the path behind the flag.
            if let Some((_, v)) = a.split_once('=')
                && !v.is_empty()
            {
                push_candidate(&mut candidates, v);
            }
            continue;
        }
        push_candidate(&mut candidates, a);
    }
    candidates
        .into_iter()
        .filter_map(|c| classify_path(&resolve_arg(cwd, c)).reason())
        .next()
}

/// Push one argv token as a path candidate, plus its de-sigilled form. curl and
/// httpie prefix an uploaded FILENAME with `@` (`-d @f`, `--data-binary @f`,
/// `-T @f`, `f@path`); without stripping it, `curl -d @~/.aws/credentials`
/// reads as the literal relative path `@~/.aws/credentials`, classifies Benign,
/// and the single most important argv this gate exists to catch walks straight
/// through. The unstripped token is kept too — a real file named `@x` is legal.
fn push_candidate<'a>(out: &mut Vec<&'a str>, tok: &'a str) {
    out.push(tok);
    if let Some(rest) = tok.strip_prefix('@')
        && !rest.is_empty()
    {
        out.push(rest);
    }
}

/// Resolve one argv token to a path the way the tool layer would: absolute as
/// given, `~`-expanded, otherwise relative to `cwd`.
fn resolve_arg(cwd: &Path, arg: &str) -> PathBuf {
    let p = Path::new(arg);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(rest) = arg.strip_prefix("~/") {
        if let Some(h) = home() {
            return h.join(rest);
        }
    } else if arg == "~"
        && let Some(h) = home()
    {
        return h;
    }
    cwd.join(p)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Make a path absolute without touching the filesystem.
fn absolutise(p: &Path) -> Option<PathBuf> {
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        return home().map(|h| h.join(rest));
    }
    if s == "~" {
        return home();
    }
    std::env::current_dir().ok().map(|c| c.join(p))
}

/// Resolve `.` and `..` textually. Never leaves a `..` in the output.
fn normalise_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalise for matching: real `fs::canonicalize` when the path exists
/// (resolving symlinks and every intermediate component), otherwise the deepest
/// existing ancestor canonicalised and the non-existent remainder re-joined with
/// lexical normalisation. A non-existent tail cannot be a symlink, so this is
/// sound for a write to a not-yet-created file.
fn resolve_for_match(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Some(c);
    }
    let comps: Vec<Component<'_>> = p.components().collect();
    for cut in (1..comps.len()).rev() {
        let prefix: PathBuf = comps[..cut].iter().map(|c| c.as_os_str()).collect();
        let Ok(canon) = std::fs::canonicalize(&prefix) else {
            continue;
        };
        let mut out = canon;
        for c in &comps[cut..] {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        return Some(out);
    }
    // Nothing on the path exists (or the root itself failed) — fall back to the
    // lexical form rather than claiming the path is unresolvable.
    if p.is_absolute() {
        Some(normalise_lexically(p))
    } else {
        None
    }
}

/// Case folding: macOS and Windows filesystems are case-insensitive by default,
/// so `.SSH` ≡ `.ssh` there; Linux is case-sensitive.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn fold(s: &str) -> String {
    s.to_lowercase()
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn fold(s: &str) -> String {
    s.to_string()
}

/// The `Normal` components of a path, case-folded for matching.
fn normal_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(fold(&s.to_string_lossy())),
            _ => None,
        })
        .collect()
}

fn match_abs(p: &Path) -> Option<&'static str> {
    let comps = normal_components(p);
    // `/proc/<pid>/environ` — the canonical form of `/proc/self/environ`.
    if comps.len() == 3 && comps[0] == "proc" && comps[2] == "environ" {
        return Some("/proc/self/environ");
    }
    SENSITIVE_ABS.iter().copied().find(|entry| {
        let want = normal_components(Path::new(entry));
        !want.is_empty() && comps.len() >= want.len() && comps[..want.len()] == want[..]
    })
}

/// True when `entry`'s components appear as a contiguous window in `comps`.
fn match_dirs(comps: &[String]) -> Option<&'static str> {
    SENSITIVE_DIRS.iter().copied().find(|entry| {
        let want: Vec<String> = entry.split('/').map(fold).collect();
        !want.is_empty()
            && comps.len() >= want.len()
            && comps.windows(want.len()).any(|w| w == want.as_slice())
    })
}

/// Glob match where `*` is the only wildcard. Case-folded like path matching.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat = fold(pattern);
    let name = fold(name);
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name;
    }
    let Some(mut rest) = name.strip_prefix(parts[0]) else {
        return false;
    };
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate().skip(1) {
        if i == last {
            return rest.len() >= part.len() && rest.ends_with(part);
        }
        match rest.find(part) {
            Some(ix) => rest = &rest[ix + part.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Decision;

    fn h(rel: &str) -> PathBuf {
        home().expect("HOME must be set for these tests").join(rel)
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aish_sensitive_{}_{}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sensitive_ssh_dir() {
        // AC1 — the canonical case, whether or not the file exists locally.
        assert!(classify_path(&h(".ssh/id_rsa")).is_sensitive());
        assert!(classify_path(&h(".ssh")).is_sensitive());
        assert!(classify_path(&h(".ssh/authorized_keys")).is_sensitive());
        assert!(classify_path(&h(".aws/credentials")).is_sensitive());
        assert!(classify_path(&h(".gnupg/secring.gpg")).is_sensitive());
        assert!(classify_path(&h(".config/gh/hosts.yml")).is_sensitive());
    }

    #[test]
    fn sensitive_globs() {
        // AC2 — file-name globs, anywhere on the filesystem.
        for name in [
            "k.pem",
            ".env",
            ".env.local",
            "id_ed25519",
            "id_rsa.pub",
            "kubeconfig",
            "server.key",
            "store.p12",
            "vpn.ovpn",
            "vault.kdbx",
            "service-account-prod.json",
            "credentials",
        ] {
            let p = Path::new("/tmp/aish_glob_probe").join(name);
            assert!(
                classify_path(&p).is_sensitive(),
                "{name} should be sensitive"
            );
        }
    }

    #[test]
    fn env_example_exempt() {
        // AC2 — committed templates are not secrets.
        for name in [
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            "app.env.example",
        ] {
            let p = Path::new("/tmp/aish_glob_probe").join(name);
            assert!(!classify_path(&p).is_sensitive(), "{name} should be benign");
        }
    }

    #[test]
    fn sensitive_abs() {
        assert!(classify_path(Path::new("/etc/shadow")).is_sensitive());
        assert!(classify_path(Path::new("/etc/sudoers")).is_sensitive());
        assert!(classify_path(Path::new("/etc/ssh/sshd_config")).is_sensitive());
        assert!(classify_path(Path::new("/var/run/docker.sock")).is_sensitive());
        assert!(classify_path(Path::new("/proc/self/environ")).is_sensitive());
        assert!(classify_path(Path::new("/root/.bashrc")).is_sensitive());
    }

    #[test]
    fn sensitive_aish_own_creds() {
        // aish protects its OWN secret store — `${profile:KEY}` reads these.
        assert!(classify_path(&h(".aish/credentials")).is_sensitive());
        assert!(classify_path(&h(".atum/credentials")).is_sensitive());
        assert!(classify_path(&h(".aish/keys/signing.key")).is_sensitive());
    }

    #[test]
    fn aish_own_config_benign() {
        // Risk-table mitigation: blanket-listing ~/.aish would make aish prompt
        // on its own session/DB/config writes and break the REPL.
        assert!(!classify_path(&h(".aish/database/aish.db")).is_sensitive());
        assert!(!classify_path(&h(".aish/.mcp.json")).is_sensitive());
        assert!(!classify_path(&h(".aish/settings.json")).is_sensitive());
        assert!(!classify_path(&h(".aish/worktrees/x/src/main.rs")).is_sensitive());
        assert!(!classify_path(&h(".atum/skills/x/SKILL.md")).is_sensitive());
    }

    #[test]
    fn traversal_normalised() {
        // AC3 — `..` must be resolved before matching, never left in place.
        assert!(classify_path(&h("x/../.ssh/id_rsa")).is_sensitive());
        assert!(classify_path(&h("./.ssh/./id_rsa")).is_sensitive());
        assert!(classify_path(&h("projects/sub/../../.aws/credentials")).is_sensitive());
    }

    #[test]
    #[cfg(unix)]
    fn symlink_resolved() {
        // AC3 — a write THROUGH a symlink into a sensitive dir is a write to it.
        let root = tmp("symlink");
        let real = root.join("fake_home/.ssh");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(classify_path(&link).is_sensitive(), "symlink itself");
        assert!(
            classify_path(&link.join("id_rsa")).is_sensitive(),
            "write through the symlink to a new file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nonexistent_write_path() {
        // AC3 — the file does not exist yet; the gate still has to see it.
        let p = h(".ssh/aish_probe_does_not_exist_new_key");
        assert!(!p.exists(), "test fixture must not exist");
        assert!(classify_path(&p).is_sensitive());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn case_insensitive_on_mac() {
        assert!(classify_path(&h(".SSH/id_rsa")).is_sensitive());
        assert!(classify_path(&h(".AWS/CREDENTIALS")).is_sensitive());
    }

    #[test]
    fn yolo_still_confirms() {
        // AC4 — the single documented exception to Yolo's "confirm nothing".
        let p = h(".ssh/id_rsa");
        assert!(matches!(
            path_gate(Mode::Yolo, false, &p),
            Gate::Confirm { .. }
        ));
        for mode in [Mode::Normal, Mode::Careful, Mode::Paranoid] {
            assert!(matches!(path_gate(mode, false, &p), Gate::Confirm { .. }));
        }
    }

    #[test]
    fn unattended_refuses() {
        // AC4 — nobody at the keyboard: refuse, never proceed.
        let p = h(".ssh/id_rsa");
        for mode in [Mode::Yolo, Mode::Normal, Mode::Careful, Mode::Paranoid] {
            assert!(matches!(path_gate(mode, true, &p), Gate::Refuse { .. }));
        }
    }

    #[test]
    fn dir_grant_does_not_cover_sensitive() {
        // AC5 — a 'd' grant on `~` must not reach `~/.ssh/authorized_keys`.
        // `grants_apply` is the ordering rule the gate consults: classify first,
        // grants second. Sensitive → grants are ignored and we still confirm.
        let target = h(".ssh/authorized_keys");
        assert!(!grants_apply(&target), "grants must not apply");
        assert!(matches!(
            path_gate(Mode::Normal, false, &target),
            Gate::Confirm { .. }
        ));
        // A benign sibling under the same granted directory is unaffected.
        assert!(grants_apply(&h("projects/readme.md")));
    }

    #[test]
    fn prompt_omits_d_option() {
        // AC5 — an option that cannot be honoured must not be displayed.
        let p = sensitive_prompt(".ssh", "write /home/u/.ssh/id_rsa");
        let (body, sensitive) = split_prompt(&p);
        assert!(sensitive);
        assert!(body.contains("sensitive path (.ssh)"));
        assert!(body.contains("write /home/u/.ssh/id_rsa"));
        assert_eq!(prompt_options(true), "[y/N]");
        assert_eq!(prompt_options(false), "[y/N/a/d]");

        // A plain prompt is untouched.
        let (body2, sensitive2) = split_prompt("write /tmp/x");
        assert!(!sensitive2);
        assert_eq!(body2, "write /tmp/x");

        // 'a' and 'd' degrade to a refusal on a sensitive prompt.
        assert_eq!(
            clamp_decision(true, Decision::AllowOnce),
            Decision::AllowOnce
        );
        assert_eq!(clamp_decision(true, Decision::AlwaysAllow), Decision::Deny);
        assert_eq!(clamp_decision(true, Decision::AllowDir), Decision::Deny);
        assert_eq!(clamp_decision(true, Decision::Deny), Decision::Deny);
        // Benign prompts keep every option.
        assert_eq!(
            clamp_decision(false, Decision::AllowDir),
            Decision::AllowDir
        );
        assert_eq!(
            clamp_decision(false, Decision::AlwaysAllow),
            Decision::AlwaysAllow
        );
    }

    #[test]
    fn read_gated_too() {
        // AC6 — reading a credential into model context IS the exfiltration path.
        assert!(matches!(
            path_gate(Mode::Normal, false, &h(".aws/credentials")),
            Gate::Confirm { .. }
        ));
        assert!(matches!(
            path_gate(Mode::Yolo, false, &h(".ssh/id_ed25519")),
            Gate::Confirm { .. }
        ));
    }

    #[test]
    fn argv_scan_catches_cat() {
        // AC6 — benign program, breaching argument.
        let cwd = Path::new("/tmp");
        assert!(
            argv_sensitivity(cwd, "cat", &["~/.ssh/id_rsa".to_string()]).is_some(),
            "cat ~/.ssh/id_rsa"
        );
        assert!(
            argv_sensitivity(
                cwd,
                "cat",
                &[h(".ssh/id_rsa").to_string_lossy().to_string()]
            )
            .is_some(),
            "absolute form"
        );
        assert!(
            argv_sensitivity(
                cwd,
                "tar",
                &["-cf".to_string(), "-".to_string(), "~/.aws".to_string()]
            )
            .is_some(),
            "tar -cf - ~/.aws"
        );
        assert!(
            argv_sensitivity(
                cwd,
                "grep",
                &["--file=~/.ssh/id_rsa".to_string(), "x".to_string()]
            )
            .is_some(),
            "path hidden behind a flag"
        );
        // Benign invocations stay silent.
        assert!(
            argv_sensitivity(cwd, "ls", &["-la".to_string()]).is_none(),
            "ls -la"
        );
        assert!(
            argv_sensitivity(
                Path::new(env!("CARGO_MANIFEST_DIR")),
                "cargo",
                &["test".to_string(), "--locked".to_string()]
            )
            .is_none(),
            "cargo test"
        );
    }

    #[test]
    fn benign_unaffected() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        for p in [
            repo.join("src/main.rs"),
            repo.join("Cargo.toml"),
            repo.join("src/sensitive.rs"),
            PathBuf::from("/tmp/x"),
            h("projects/app/README.md"),
        ] {
            assert!(
                !classify_path(&p).is_sensitive(),
                "{} should be benign, got {:?}",
                p.display(),
                classify_path(&p).reason()
            );
        }
        assert!(matches!(
            path_gate(Mode::Yolo, false, &repo.join("src/main.rs")),
            Gate::Normal
        ));
        assert!(matches!(
            path_gate(Mode::Normal, true, &repo.join("Cargo.toml")),
            Gate::Normal
        ));
    }

    #[test]
    fn glob_matcher_basics() {
        assert!(glob_match("*.pem", "key.pem"));
        assert!(!glob_match("*.pem", "pem"));
        assert!(glob_match("id_rsa*", "id_rsa.pub"));
        assert!(glob_match(
            "service-account*.json",
            "service-account-x.json"
        ));
        assert!(!glob_match("service-account*.json", "sa.json"));
        assert!(glob_match(".env", ".env"));
        assert!(!glob_match(".env", ".environment"));
    }
}
