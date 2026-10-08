//! Central secret-redaction filter (TASK-947 — SEC-3.2 / F-02).
//!
//! Defence-in-depth against secret leakage. aish resolves credential
//! references (`${profile:KEY}`, `${ENV}`) into the environment of programs it
//! execs; a careless `env`, a `curl -v`, a crashing tool that echoes its
//! config, or a `read_file ~/.aws/credentials` would otherwise splash that
//! plaintext straight into (a) the model's context, (b) the on-disk
//! transcript, and (c) the SQLite history DB — three durable copies we can
//! never recall.
//!
//! This module is the single chokepoint that scrubs captured output BEFORE it
//! reaches any of those sinks. Two complementary strategies:
//!
//! 1. **Registry** — exact values aish itself resolved. `register(value,
//!    label)` records a known secret; every later `scrub` replaces it with a
//!    visible `[redacted:<label>]` marker. Values shorter than
//!    [`MIN_SECRET_LEN`] and obvious non-secrets ([`LOW_ENTROPY_DENY`]) are
//!    refused, so registering `PORT=8080` can't turn every `8080` in the
//!    output into noise.
//! 2. **Patterns** — well-known token shapes (GitHub PAT, AWS access key,
//!    `sk-ant-…`, JWT, PEM private key, `postgres://user:pass@…`, …) that we
//!    never saw resolved but recognise on sight. Compiled once behind a
//!    [`OnceLock`] with a [`RegexSet`] pre-filter so the common case (clean
//!    output) costs one linear pass and zero allocations.
//!
//! [`Redactor::scrub`] returns [`Cow::Borrowed`] when nothing matched — the
//! zero-copy fast path that keeps the filter viable on every captured byte.
//!
//! Ordering matters: callers MUST scrub **before** truncating. A secret that
//! straddles the head/tail boundary of a middle-truncated capture would
//! otherwise survive as two halves that no later pass can recognise.

use std::borrow::Cow;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use regex::{Regex, RegexSet};

/// Shortest value the registry will accept. Anything shorter is more likely a
/// port, a flag, or a word than a credential, and redacting it would corrupt
/// far more output than it protects.
pub const MIN_SECRET_LEN: usize = 8;

/// Literal that must appear in the haystack before the (lazy, unbounded) PEM
/// pattern is allowed to run. Cheap `memchr`-backed guard.
const PEM_MARKER: &str = "-----BEGIN";

/// Values that pass the length bar but are plainly not secrets. Matched
/// case-insensitively against the whole trimmed value.
pub const LOW_ENTROPY_DENY: &[&str] = &[
    "password",
    "passwd",
    "changeme",
    "change-me",
    "secret",
    "secrets",
    "localhost",
    "127.0.0.1",
    "0.0.0.0",
    "true",
    "false",
    "enabled",
    "disabled",
    "admin",
    "administrator",
    "root",
    "default",
    "example",
    "undefined",
    "none",
    "null",
    "development",
    "production",
    "staging",
    "12345678",
    "123456789",
    "00000000",
    "abcdefgh",
    "test1234",
    "untitled",
];

/// A registered secret and the human label shown in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Secret {
    value: String,
    label: String,
}

/// The redaction registry: known secret values plus the session-local on/off
/// switch and a match counter for `:redact` status.
#[derive(Debug)]
pub struct Redactor {
    /// Sorted longest-value-first so a longer secret is replaced before any
    /// shorter secret that happens to be its prefix.
    values: Vec<Secret>,
    enabled: bool,
    hits: AtomicU64,
}

impl Default for Redactor {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            enabled: true,
            hits: AtomicU64::new(0),
        }
    }
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `value` as a secret to scrub, shown as `[redacted:<label>]`.
    ///
    /// Returns `false` when the value is REFUSED — too short
    /// ([`MIN_SECRET_LEN`]) or low-entropy ([`LOW_ENTROPY_DENY`]) — so the
    /// caller can tell "tracked" from "ignored". Idempotent: re-registering an
    /// already-tracked value returns `true` without duplicating it.
    ///
    /// Also registers encoded forms (base64, percent-encoding) under the same
    /// label, so a secret that travels through a URL or a JSON blob is still
    /// caught.
    pub fn register(&mut self, value: &str, label: &str) -> bool {
        let v = value.trim();
        if v.len() < MIN_SECRET_LEN || is_low_entropy(v) {
            return false;
        }
        let label = {
            let l = label.trim();
            if l.is_empty() { "secret" } else { l }
        };
        self.insert(v.to_string(), label);
        for enc in encodings(v) {
            self.insert(enc, label);
        }
        true
    }

    fn insert(&mut self, value: String, label: &str) {
        if value.len() < MIN_SECRET_LEN || self.values.iter().any(|s| s.value == value) {
            return;
        }
        self.values.push(Secret {
            value,
            label: label.to_string(),
        });
        // Longest first: a secret that contains another secret must win.
        self.values.sort_by(|a, b| {
            b.value
                .len()
                .cmp(&a.value.len())
                .then(a.value.cmp(&b.value))
        });
    }

    /// Scrub registered values and known token patterns out of `s`.
    ///
    /// Returns [`Cow::Borrowed`] (no allocation, no copy) when nothing
    /// matched — the overwhelmingly common case.
    pub fn scrub<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if !self.enabled || s.is_empty() {
            return Cow::Borrowed(s);
        }

        let mut cur: Cow<'a, str> = Cow::Borrowed(s);

        // (1) Exact known values.
        for sec in &self.values {
            if !cur.contains(sec.value.as_str()) {
                continue;
            }
            let n = cur.matches(sec.value.as_str()).count() as u64;
            self.hits.fetch_add(n, Ordering::Relaxed);
            let marker = marker(&sec.label);
            let next = cur.replace(sec.value.as_str(), &marker);
            cur = Cow::Owned(next);
        }

        // (2) Token shapes. One RegexSet pass tells us which (if any) of the
        // patterns can match at all; only those get a replace_all.
        let p = patterns();
        let idxs: Vec<usize> = p.set.matches(cur.as_ref()).into_iter().collect();
        for i in idxs {
            // Guard the lazy, unbounded PEM body against pathological input:
            // never run it unless the literal header is actually present.
            if i == p.pem_index && !cur.contains(PEM_MARKER) {
                continue;
            }
            let n = p.regs[i].find_iter(cur.as_ref()).count() as u64;
            if n == 0 {
                continue;
            }
            self.hits.fetch_add(n, Ordering::Relaxed);
            let marker = marker(p.labels[i]);
            let next = p.regs[i]
                .replace_all(cur.as_ref(), marker.as_str())
                .into_owned();
            cur = Cow::Owned(next);
        }

        cur
    }

    /// Session-local escape valve (`:redact off`). Never persisted.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Number of tracked values (including encoded variants).
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Distinct labels, in registration-independent sorted order. Labels only —
    /// a status line must never echo a value.
    pub fn labels(&self) -> Vec<String> {
        let mut v: Vec<String> = self.values.iter().map(|s| s.label.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Total replacements performed since the session started.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }
}

fn marker(label: &str) -> String {
    format!("[redacted:{label}]")
}

/// Whole-value low-entropy check (case-insensitive deny list, plus "one
/// character repeated", which is never a credential).
fn is_low_entropy(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    if LOW_ENTROPY_DENY.contains(&lower.as_str()) {
        return true;
    }
    let mut chars = v.chars();
    match chars.next() {
        None => true,
        Some(first) => chars.all(|c| c == first),
    }
}

/// Encoded forms of a secret worth tracking alongside the literal value.
fn encodings(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = base64(v.as_bytes());
    if b != v {
        out.push(b);
    }
    let p = percent_encode(v);
    if p != v {
        out.push(p);
    }
    out
}

/// Standard base64 (with padding). Local so the filter adds no dependency.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b1 = *c.first().unwrap_or(&0) as u32;
        let b2 = *c.get(1).unwrap_or(&0) as u32;
        let b3 = *c.get(2).unwrap_or(&0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
fn percent_encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Token patterns
// ---------------------------------------------------------------------------

struct Patterns {
    set: RegexSet,
    regs: Vec<Regex>,
    labels: Vec<&'static str>,
    pem_index: usize,
}

/// `(label, pattern)` pairs. Order is the replacement order: more specific
/// shapes first so a narrow label wins over a broad one (e.g. `sk-ant-…` is
/// labelled `anthropic-key`, not `openai-key`).
const TOKEN_PATTERNS: &[(&str, &str)] = &[
    (
        "private-key-pem",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
    ),
    ("github-fine-grained", r"\bgithub_pat_[A-Za-z0-9_]{22,255}"),
    ("github-token", r"\bgh[pousr]_[A-Za-z0-9]{16,255}"),
    ("aws-access-key", r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b"),
    ("anthropic-key", r"\bsk-ant-[A-Za-z0-9_-]{16,}"),
    ("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9]{20,}\b"),
    ("slack-token", r"\bxox[baprse]-[A-Za-z0-9-]{10,}"),
    ("google-api-key", r"\bAIza[0-9A-Za-z_-]{35}"),
    ("stripe-key", r"\b[sprk]k_(?:live|test)_[A-Za-z0-9]{10,}"),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}",
    ),
    (
        "bearer-token",
        r"(?i)\b(?:authorization|bearer|api[_-]?key|x-api-key)\b\s*[:=]?\s*(?:bearer\s+)?[A-Za-z0-9._~+/=-]{16,}",
    ),
    (
        "db-url",
        r"\b(?:postgres|postgresql|mysql|mariadb|mongodb\+srv|mongodb|redis|rediss|amqps|amqp|clickhouse)://[^\s:/@]+:[^\s/@]+@\S+",
    ),
];

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let labels: Vec<&'static str> = TOKEN_PATTERNS.iter().map(|(l, _)| *l).collect();
        let srcs: Vec<&'static str> = TOKEN_PATTERNS.iter().map(|(_, r)| *r).collect();
        let set = RegexSet::new(&srcs).expect("TOKEN_PATTERNS compile");
        let regs = srcs
            .iter()
            .map(|r| Regex::new(r).expect("TOKEN_PATTERNS compile"))
            .collect();
        let pem_index = labels
            .iter()
            .position(|l| *l == "private-key-pem")
            .expect("pem pattern present");
        Patterns {
            set,
            regs,
            labels,
            pem_index,
        }
    })
}

// ---------------------------------------------------------------------------
// Process-wide registry
// ---------------------------------------------------------------------------

/// The shared registry. `Arc<Mutex<_>>` so the capture path, the tool layer and
/// the `:redact` command all see one view.
pub fn registry() -> &'static Arc<Mutex<Redactor>> {
    static R: OnceLock<Arc<Mutex<Redactor>>> = OnceLock::new();
    R.get_or_init(|| Arc::new(Mutex::new(Redactor::new())))
}

/// Lock the registry, recovering from poisoning. A panic elsewhere must never
/// turn redaction OFF — failing closed here means leaking secrets.
fn lock() -> MutexGuard<'static, Redactor> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a resolved secret with the process registry. Returns `false` when
/// refused (too short / low entropy).
pub fn register_secret(value: &str, label: &str) -> bool {
    lock().register(value, label)
}

/// Scrub a borrowed string. Zero-copy when nothing matched.
pub fn scrub<'a>(s: &'a str) -> Cow<'a, str> {
    lock().scrub(s)
}

/// Scrub an owned string in place, reusing the allocation when nothing matched.
pub fn scrub_owned(s: String) -> String {
    match lock().scrub(&s) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => o,
    }
}

/// True when redaction is active for this session.
pub fn enabled() -> bool {
    lock().enabled()
}

// ---------------------------------------------------------------------------
// Sensitive paths
// ---------------------------------------------------------------------------

/// Path fragments that mark a file as credential-bearing. Matched on the full
/// path, lower-cased, with `\` normalised to `/`.
const SENSITIVE_FRAGMENTS: &[&str] = &[
    "/.aws/credentials",
    "/.aws/config",
    "/.atum/credentials",
    "/.config/gh/hosts.yml",
    "/.docker/config.json",
    "/.git-credentials",
    "/.kube/config",
    "/.netrc",
    "/.npmrc",
    "/.pgpass",
    "/.ssh/",
    "credentials.json",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
    "service-account",
];

/// File names/suffixes that mark a file as credential-bearing.
const SENSITIVE_SUFFIXES: &[&str] = &[".env", ".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];

/// True when a path is credential-bearing and its CONTENT should be scrubbed.
///
/// Deliberately narrow: an ordinary source file must come back from
/// `read_file` byte-identical, so only paths that look like credential stores
/// are filtered.
pub fn is_sensitive_path(path: &Path) -> bool {
    let p = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if SENSITIVE_FRAGMENTS.iter().any(|f| p.contains(f)) {
        return true;
    }
    if SENSITIVE_SUFFIXES.iter().any(|s| p.ends_with(s)) {
        return true;
    }
    // `.env`, `.env.local`, `.env.production`, …
    let name = p.rsplit('/').next().unwrap_or(&p);
    name == ".env" || name.starts_with(".env.") || name.ends_with(".env")
}

// ---------------------------------------------------------------------------
// `:redact` command
// ---------------------------------------------------------------------------

/// True when this session is UNATTENDED — a `yolo`-mode session or a
/// background coordinator (`session.nested`). No human is watching such a
/// session, so it may not disable redaction.
pub fn is_unattended(mode: crate::session::Mode, nested: bool) -> bool {
    nested || mode == crate::session::Mode::Yolo
}

/// Handle `:redact [on|off|status]`. Returns the text to print.
///
/// `off` is REFUSED in an unattended session: the whole point of the filter is
/// that nobody is reading a coordinator's transcript before it lands in the DB.
pub fn handle_redact(arg: Option<&str>, unattended: bool) -> String {
    match arg.map(str::trim).unwrap_or("") {
        "" | "status" => status_line(),
        "off" => {
            if unattended {
                tracing::warn!(
                    "redact: refused `:redact off` in an unattended/coordinator session"
                );
                return "refused: `:redact off` is not available in an unattended session \
                        (yolo mode / background coordinator) — nobody is reading the transcript \
                        before it is persisted.\n"
                    .to_string()
                    + &status_line();
            }
            lock().set_enabled(false);
            tracing::warn!("redact: secret redaction DISABLED for this session by `:redact off`");
            "redaction → off — secrets will NOT be scrubbed from captured output for the rest of \
             this session (`:redact on` to restore)"
                .to_string()
        }
        "on" => {
            let was_on = enabled();
            lock().set_enabled(true);
            if was_on {
                "redaction → on (already on)".to_string()
            } else {
                "redaction → on".to_string()
            }
        }
        other => format!("unknown: `:redact {other}`\nusage: :redact [status|on|off]"),
    }
}

/// Status text. Labels and counts only — NEVER a value.
fn status_line() -> String {
    let g = lock();
    let tracked = if g.is_empty() {
        String::new()
    } else {
        format!(" [{}]", g.labels().join(", "))
    };
    format!(
        "redaction: {} — {} value(s) tracked{tracked}; {} replacement(s) made",
        if g.enabled() { "on" } else { "off" },
        g.len(),
        g.hits()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the few tests that touch the PROCESS-WIDE registry.
    fn global_lock() -> MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn reset_global() {
        let mut g = lock();
        *g = Redactor::new();
    }

    // -- registry ----------------------------------------------------------

    #[test]
    fn scrub_registered_value() {
        let mut r = Redactor::new();
        assert!(r.register("s3cr3t-value-abcdef", "profile:TOKEN"));
        let out = r.scrub("token=s3cr3t-value-abcdef done");
        assert_eq!(out, "token=[redacted:profile:TOKEN] done");
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn short_value_not_registered() {
        let mut r = Redactor::new();
        assert!(!r.register("abc1234", "SHORT")); // 7 chars < MIN_SECRET_LEN
        assert!(r.is_empty());
        assert_eq!(r.scrub("abc1234"), "abc1234");
    }

    #[test]
    fn low_entropy_denied() {
        let mut r = Redactor::new();
        assert!(!r.register("password", "PW"));
        assert!(!r.register("LOCALHOST", "HOST")); // case-insensitive
        assert!(!r.register("aaaaaaaaaa", "REPEAT")); // one char repeated
        assert!(r.is_empty());
    }

    #[test]
    fn longest_value_wins() {
        let mut r = Redactor::new();
        r.register("abcdefgh1234", "LONG");
        r.register("abcdefgh", "SHORT");
        assert_eq!(r.scrub("x abcdefgh1234 y"), "x [redacted:LONG] y");
    }

    #[test]
    fn register_is_idempotent() {
        let mut r = Redactor::new();
        assert!(r.register("repeated-secret-1", "A"));
        let n = r.len();
        assert!(r.register("repeated-secret-1", "A"));
        assert_eq!(r.len(), n);
    }

    #[test]
    fn encoded_forms_are_scrubbed() {
        let mut r = Redactor::new();
        let secret = "tok/en+with=chars";
        r.register(secret, "ENC");
        let b64 = base64(secret.as_bytes());
        let pct = percent_encode(secret);
        assert_eq!(r.scrub(&b64), "[redacted:ENC]");
        assert_eq!(r.scrub(&pct), "[redacted:ENC]");
    }

    #[test]
    fn base64_matches_reference_vectors() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    // -- patterns ----------------------------------------------------------

    fn scrubbed(s: &str) -> String {
        Redactor::new().scrub(s).into_owned()
    }

    fn assert_pattern(sample: &str, label: &str) {
        let out = scrubbed(sample);
        assert!(
            out.contains(&format!("[redacted:{label}]")),
            "{sample:?} → {out:?} (expected label {label})"
        );
        assert!(!out.contains(sample), "raw sample survived: {out:?}");
    }

    // Pattern fixtures are assembled at runtime from inert fragments so that
    // provider-side secret scanners (and GitHub push protection) do not flag
    // this test file for carrying a literal token-shaped string.
    fn fixture(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn pattern_github_token() {
        assert_pattern(
            &fixture(&["ghp", "_abcdefghij0123456789ABCDEFGHIJ0123"]),
            "github-token",
        );
    }

    #[test]
    fn pattern_github_fine_grained() {
        assert_pattern(
            &fixture(&[
                "github",
                "_pat_11ABCDEFG0abcdefghij_KLMNOPqrstuvwx0123456789ABCDEFGH",
            ]),
            "github-fine-grained",
        );
    }

    #[test]
    fn pattern_aws_access_key() {
        assert_pattern("AKIAIOSFODNN7EXAMPLE", "aws-access-key");
        assert_pattern("ASIAIOSFODNN7EXAMPLE", "aws-access-key");
    }

    #[test]
    fn pattern_anthropic_key() {
        assert_pattern(
            &fixture(&["sk-", "ant-api03-AbCdEf0123456789_xyz-ABC"]),
            "anthropic-key",
        );
    }

    #[test]
    fn pattern_openai_key() {
        assert_pattern(
            &fixture(&["sk-", "abcdefghij0123456789ABCDEFGHIJ"]),
            "openai-key",
        );
        assert_pattern(
            &fixture(&["sk-", "proj-abcdefghij0123456789ABCDEF"]),
            "openai-key",
        );
    }

    #[test]
    fn pattern_slack_token() {
        assert_pattern(
            &fixture(&["xox", "b-123456789012-abcdefABCDEF0123"]),
            "slack-token",
        );
    }

    #[test]
    fn pattern_google_api_key() {
        assert_pattern(
            &fixture(&["AIza", "SyA0123456789abcdefghijklmnopqrstuv"]),
            "google-api-key",
        );
    }

    #[test]
    fn pattern_stripe_key() {
        assert_pattern(
            &fixture(&["sk_", "live_abcdefghij0123456789"]),
            "stripe-key",
        );
    }

    #[test]
    fn pattern_jwt() {
        assert_pattern(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1g",
            "jwt",
        );
    }

    #[test]
    fn pattern_pem_private_key() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0abcdef\nghijkl\n-----END RSA PRIVATE KEY-----";
        let out = scrubbed(&format!("before\n{pem}\nafter"));
        assert_eq!(out, "before\n[redacted:private-key-pem]\nafter");
    }

    #[test]
    fn pattern_bearer_authorization() {
        let out = scrubbed("Authorization: Bearer abcdefghijklmnopqrstuvwx");
        assert!(out.contains("[redacted:bearer-token]"), "{out:?}");
        assert!(!out.contains("abcdefghijklmnopqrstuvwx"));
    }

    #[test]
    fn pattern_db_url() {
        let out = scrubbed("DATABASE_URL=postgres://user:sup3rs3cret@db.internal:5432/app");
        assert!(out.contains("[redacted:db-url]"), "{out:?}");
        assert!(!out.contains("sup3rs3cret"));
    }

    #[test]
    fn anthropic_key_not_labelled_openai() {
        let out = scrubbed(&fixture(&["sk-", "ant-api03-AbCdEf0123456789_xyz-ABC"]));
        assert!(out.contains("[redacted:anthropic-key]"), "{out:?}");
        assert!(!out.contains("openai-key"), "{out:?}");
    }

    // -- fast path / ordering ---------------------------------------------

    #[test]
    fn clean_output_zero_copy() {
        let mut r = Redactor::new();
        r.register("some-real-secret-value", "TOK");
        let clean = "cargo test\n   Compiling aish v0.1.0\n    Finished in 1.2s\nok";
        let out = r.scrub(clean);
        assert!(
            matches!(out, Cow::Borrowed(_)),
            "clean output must not allocate"
        );
        assert_eq!(out, clean);
        assert_eq!(r.hits(), 0);
    }

    #[test]
    fn disabled_redactor_is_passthrough() {
        let mut r = Redactor::new();
        r.register("some-real-secret-value", "TOK");
        r.set_enabled(false);
        let s = "leak some-real-secret-value here";
        assert!(matches!(r.scrub(s), Cow::Borrowed(_)));
        assert_eq!(r.scrub(s), s);
    }

    /// The ordering guarantee: scrub BEFORE truncation. A secret straddling the
    /// head/tail boundary of a middle-truncated capture would otherwise be cut
    /// in half and survive in two unrecognisable pieces.
    #[test]
    fn scrub_before_truncation() {
        let mut r = Redactor::new();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        r.register(secret, "AWS_KEY");

        // Build output where the secret straddles the truncation seam exactly:
        // `truncate_middle` keeps the first `max * 3 / 4` bytes as the head, so
        // starting the key 10 bytes before that leaves half of it in the head
        // and throws the other half away with the middle.
        let max = 200usize;
        let seam = max * 3 / 4;
        let mut raw = String::new();
        raw.push_str(&"a".repeat(seam - 10));
        raw.push_str(secret);
        raw.push_str(&"b".repeat(600));

        // Right order: scrub, then truncate → no fragment survives.
        let scrubbed_then_cut = crate::tools::truncate_middle(r.scrub(&raw).into_owned(), max);
        assert!(!scrubbed_then_cut.contains(secret));
        assert!(!scrubbed_then_cut.contains(&secret[..10]));

        // Wrong order (truncate first) leaves half the key in the head — this is
        // what the production ordering exists to prevent.
        let cut_then_scrubbed = r
            .scrub(&crate::tools::truncate_middle(raw.clone(), max))
            .into_owned();
        assert!(
            cut_then_scrubbed.contains(&secret[..10]),
            "fixture no longer straddles the seam: {cut_then_scrubbed:?}"
        );
    }

    // -- paths -------------------------------------------------------------

    #[test]
    fn sensitive_paths_detected() {
        for p in [
            "/home/u/.aws/credentials",
            "/home/u/.ssh/id_rsa",
            "/home/u/.atum/credentials",
            "/srv/app/.env",
            "/srv/app/.env.production",
            "/etc/ssl/server.pem",
            "/home/u/.netrc",
            "/home/u/.git-credentials",
            "/home/u/secrets.yaml",
        ] {
            assert!(is_sensitive_path(Path::new(p)), "{p} should be sensitive");
        }
    }

    #[test]
    fn benign_file_read_untouched() {
        for p in ["src/main.rs", "/repo/README.md", "Cargo.toml", "env.rs"] {
            assert!(!is_sensitive_path(Path::new(p)), "{p} must stay benign");
        }
        // And the filter itself leaves ordinary source byte-identical.
        let src = "fn main() {\n    let key = std::env::var(\"API_KEY\");\n}\n";
        let r = Redactor::new();
        assert!(matches!(r.scrub(src), Cow::Borrowed(_)));
        assert_eq!(r.scrub(src), src);
    }

    // -- performance / robustness -----------------------------------------

    #[test]
    fn bench_scrub_1mb_clean() {
        let mut r = Redactor::new();
        for i in 0..16 {
            r.register(&format!("registered-secret-value-{i:03}"), "TOK");
        }
        let chunk = "2026-01-01T00:00:00Z INFO compiling crate number 1234 ok\n";
        let blob = chunk.repeat(1_000_000 / chunk.len() + 1);
        assert!(blob.len() >= 1_000_000);

        let t0 = std::time::Instant::now();
        let out = r.scrub(&blob);
        let dt = t0.elapsed();
        assert!(matches!(out, Cow::Borrowed(_)));
        // Generous bound: the point is "linear, not pathological". Debug builds
        // on a loaded CI box are slow, so allow 3s before calling it a defect.
        assert!(dt.as_secs_f64() < 3.0, "1 MB clean scrub took {dt:?}");
    }

    #[test]
    fn no_catastrophic_backtracking() {
        let r = Redactor::new();
        // A long run of the PEM header prefix with no END: the lazy unbounded
        // body is the one pattern that could blow up. Must return promptly.
        let evil = format!("{}{}", "-----BEGIN ".repeat(2_000), "A".repeat(200_000));
        let t0 = std::time::Instant::now();
        let out = r.scrub(&evil);
        assert!(t0.elapsed().as_secs_f64() < 3.0);
        assert_eq!(out, evil); // unterminated PEM is left alone

        // Same for a near-miss JWT / bearer soup.
        let evil2 = "eyJ".repeat(50_000);
        let t0 = std::time::Instant::now();
        let _ = r.scrub(&evil2);
        assert!(t0.elapsed().as_secs_f64() < 3.0);
    }

    #[test]
    fn pem_literal_precheck_skips_regex() {
        // No `-----BEGIN` anywhere → the PEM pattern never runs, output clean.
        let r = Redactor::new();
        let s = "PRIVATE KEY-----\nnot a pem at all\n-----END RSA PRIVATE KEY-----";
        assert_eq!(r.scrub(s), s);
    }

    // -- `:redact` command -------------------------------------------------

    #[test]
    fn redact_off_refused_unattended() {
        let _g = global_lock();
        reset_global();
        let out = handle_redact(Some("off"), true);
        assert!(out.contains("refused"), "{out:?}");
        assert!(enabled(), "redaction must stay ON in an unattended session");
        reset_global();
    }

    #[test]
    fn redact_off_allowed_interactive() {
        let _g = global_lock();
        reset_global();
        let out = handle_redact(Some("off"), false);
        assert!(out.contains("redaction → off"), "{out:?}");
        assert!(!enabled());
        let out = handle_redact(Some("on"), false);
        assert!(out.contains("redaction → on"), "{out:?}");
        assert!(enabled());
        reset_global();
    }

    #[test]
    fn redact_status_hides_values() {
        let _g = global_lock();
        reset_global();
        assert!(register_secret(
            "hunter2-but-longer-abc",
            "profile:ATUM_KEY"
        ));
        let scrubbed = scrub_owned("x hunter2-but-longer-abc y".to_string());
        assert_eq!(scrubbed, "x [redacted:profile:ATUM_KEY] y");

        let out = handle_redact(None, false);
        assert!(out.contains("profile:ATUM_KEY"), "{out:?}");
        assert!(!out.contains("hunter2-but-longer-abc"), "LEAK: {out:?}");
        assert!(out.contains("replacement(s) made"), "{out:?}");
        reset_global();
    }

    #[test]
    fn redact_usage_on_garbage() {
        let _g = global_lock();
        let out = handle_redact(Some("sideways"), false);
        assert!(out.contains("usage: :redact"), "{out:?}");
    }

    #[test]
    fn unattended_detection() {
        use crate::session::Mode;
        assert!(is_unattended(Mode::Yolo, false));
        assert!(is_unattended(Mode::Normal, true)); // background coordinator
        assert!(!is_unattended(Mode::Normal, false));
        assert!(!is_unattended(Mode::Paranoid, false));
    }

    #[test]
    fn scrub_owned_reuses_allocation_when_clean() {
        let _g = global_lock();
        reset_global();
        let s = "nothing to see here".to_string();
        let ptr = s.as_ptr();
        let out = scrub_owned(s);
        assert_eq!(out.as_ptr(), ptr, "clean scrub_owned must not reallocate");
        reset_global();
    }
}
