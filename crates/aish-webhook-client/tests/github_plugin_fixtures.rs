//! TASK-373 (SPR-104) — fixture-driven tests for the shipped GitHub plugin.
//!
//! Loads the REAL `plugins/github/plugin.json` through
//! [`PluginRegistry::load_dir`] (so relative handler paths resolve exactly as
//! in production), dispatches recorded-shape GitHub payloads from
//! `plugins/github/tests/fixtures/` through [`WebhookDispatcher`] (real
//! fork/exec, no shell), and asserts on each handler's one-line summary.
//!
//! The opt-in PR review dispatch (`GITHUB_PR_AUTOREVIEW=1`) is exercised by
//! running `handlers/pr-review.sh` directly with a stub `aish` on `PATH` and an
//! isolated `TMPDIR`, so no real agent is ever launched.
//!
//! Hermetic: no sockets, builds under `--no-default-features`. Needs `bash` and
//! `python3`, which the handlers themselves require.
//!
//! NOT covered here: the live broker leg (GitHub-signed delivery → broker →
//! client). That end-to-end test depends on the TASK-449 protocol fix.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aish_webhook_client::{HandlerOutcome, PluginRegistry, Webhook, WebhookDispatcher};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root")
}

fn github_plugin_dir() -> PathBuf {
    repo_root().join("plugins").join("github")
}

fn fixture(name: &str) -> serde_json::Value {
    let p = github_plugin_dir()
        .join("tests")
        .join("fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// A private, dependency-free temp dir.
fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "aish-gh-fixtures-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Registry rooted at a temp dir that holds ONLY a symlink to the real github
/// plugin, so sibling plugins under `plugins/` (some subscribe to `"*"`) never
/// run as a side effect of these tests.
fn github_registry() -> PluginRegistry {
    let root = tempdir("root");
    #[cfg(unix)]
    std::os::unix::fs::symlink(github_plugin_dir(), root.join("github")).unwrap();
    let reg = PluginRegistry::load_dir(&root).expect("load_dir");
    assert_eq!(reg.len(), 1, "exactly the github plugin is loaded");
    reg
}

async fn dispatch(event_type: &str, payload: serde_json::Value) -> Vec<HandlerOutcome> {
    let dispatcher = WebhookDispatcher::new(Arc::new(github_registry()));
    let wh = Webhook {
        id: "w-gh-fixture".into(),
        tenant_id: "t_fixture".into(),
        plugin_id: String::new(),
        event_type: event_type.into(),
        payload,
    };
    dispatcher
        .dispatch(&wh)
        .await
        .into_iter()
        .filter(|o| o.plugin_id == "github" && o.executed)
        .collect()
}

/// Dispatch and require exactly one executed, successful handler with a
/// single-line stdout; returns that line.
async fn one_line(event_type: &str, payload: serde_json::Value) -> String {
    let outs = dispatch(event_type, payload).await;
    assert_eq!(outs.len(), 1, "exactly one handler executes: {outs:#?}");
    let o = &outs[0];
    assert!(o.success, "handler exit 0: {o:#?}");
    let out = o.stdout.trim_end();
    assert_eq!(out.lines().count(), 1, "one summary line, got {out:?}");
    out.to_string()
}

#[test]
fn manifest_declares_all_github_events() {
    let reg = github_registry();
    let mut events = BTreeSet::new();
    for ev in ["push", "pull_request", "issues", "workflow_run", "release"] {
        let m = reg.matching(ev);
        assert!(!m.is_empty(), "github plugin subscribes to {ev}");
        for (pid, h) in m {
            assert_eq!(pid, "github");
            let prog = Path::new(&h.command[0]);
            assert!(prog.is_absolute(), "command resolved: {}", prog.display());
            assert!(prog.is_file(), "handler exists: {}", prog.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(prog).unwrap().permissions().mode();
                assert!(mode & 0o111 != 0, "handler executable: {}", prog.display());
            }
            events.insert(h.event_type.clone());
        }
    }
    assert_eq!(events.len(), 5);
}

#[tokio::test]
async fn push_fixture_summarised() {
    let line = one_line("push", fixture("push.json")).await;
    for needle in [
        "[github/push] acme/widgets branch main:",
        "3 commits 6113728..0d1a26e",
        "by @octocat",
        "tenant=t_fixture",
        ": Fix widget lookup off-by-one ",
        "https://github.com/acme/widgets/compare/",
    ] {
        assert!(line.contains(needle), "missing {needle:?} in {line:?}");
    }
    // Only the head commit's FIRST line is surfaced.
    assert!(!line.contains("1-based"), "{line}");
}

#[tokio::test]
async fn push_branch_deleted_fixture() {
    let line = one_line("push", fixture("push-branch-deleted.json")).await;
    assert_eq!(
        line,
        "[github/push] acme/widgets branch feat/old-widget deleted by @hubot tenant=t_fixture"
    );
}

#[tokio::test]
async fn push_tag_fixture() {
    let line = one_line("push", fixture("push-tag.json")).await;
    assert!(
        line.starts_with("[github/push] acme/widgets tag v1.4.0: 0 commits 0000000..0d1a26e"),
        "{line}"
    );
    assert!(line.contains("[new]"), "{line}");
}

#[tokio::test]
async fn issues_opened_fixture() {
    let line = one_line("issues", fixture("issues-opened.json")).await;
    assert_eq!(
        line,
        "[github/issue] acme/widgets#1347 opened by @octocat [open] labels=bug,registry \
         tenant=t_fixture: Widget registry panics on empty config \
         https://github.com/acme/widgets/issues/1347"
    );
}

#[tokio::test]
async fn issues_labeled_fixture() {
    let line = one_line("issues", fixture("issues-labeled.json")).await;
    assert!(
        line.starts_with("[github/issue] acme/widgets#1348 labeled +label:priority:high by @hubot"),
        "{line}"
    );
    assert!(line.contains("labels=docs,priority:high"), "{line}");
}

#[tokio::test]
async fn issues_closed_fixture() {
    let line = one_line("issues", fixture("issues-closed.json")).await;
    assert!(
        line.starts_with("[github/issue] acme/widgets#1347 closed by @octocat [closed] labels=-"),
        "{line}"
    );
}

#[tokio::test]
async fn issues_unsubscribed_action_not_executed() {
    let outs = dispatch("issues", fixture("issues-assigned.json")).await;
    assert!(outs.is_empty(), "`assigned` is filtered out: {outs:#?}");
}

#[tokio::test]
async fn pr_opened_fixture_summary() {
    let line = one_line("pull_request", fixture("pull_request-opened.json")).await;
    assert_eq!(
        line,
        "[github/pr] acme/widgets#42 opened by @octocat (feat/widget\u{2192}main) \
         tenant=t_fixture: Add widget https://github.com/acme/widgets/pull/42"
    );
}

/// Shell-injection guard: payload strings full of shell metacharacters are
/// printed verbatim and never executed by any handler.
#[tokio::test]
async fn hostile_strings_printed_verbatim_never_executed() {
    let dir = tempdir("hostile");
    let canary = dir.join("PWNED");
    let c = canary.display().to_string();
    let hostile = format!("$(touch {c}); `touch {c}` ; touch {c} 'sq' \"dq\" \\");

    let mut issue = fixture("issues-opened.json");
    issue["issue"]["title"] = hostile.clone().into();
    issue["issue"]["user"]["login"] = format!("x$(touch {c})").into();
    let line = one_line("issues", issue).await;
    assert!(line.contains(&hostile), "verbatim title in {line:?}");

    let mut push = fixture("push.json");
    push["ref"] = format!("refs/heads/$(touch {c})").into();
    push["head_commit"]["message"] = format!("`touch {c}`").into();
    one_line("push", push).await;

    let mut pr = fixture("pull_request-opened.json");
    pr["pull_request"]["title"] = hostile.clone().into();
    pr["pull_request"]["head"]["ref"] = format!("x';touch {c};'").into();
    let line = one_line("pull_request", pr).await;
    assert!(line.contains(&hostile), "verbatim title in {line:?}");

    assert!(!canary.exists(), "payload text was executed");
}

// ---------------------------------------------------------------------------
// Opt-in PR review dispatch (GITHUB_PR_AUTOREVIEW=1)
// ---------------------------------------------------------------------------

struct ReviewSandbox {
    dir: PathBuf,
    calls: PathBuf,
    path_env: String,
}

impl ReviewSandbox {
    fn new(tag: &str) -> Self {
        let dir = tempdir(tag);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let calls = dir.join("calls");
        let stub = bin.join("aish");
        // Stub records one line per invocation: its argv.
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AISH_STUB_CALLS\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            dir,
            calls,
            path_env,
        }
    }

    /// Run pr-review.sh on `payload`; `flag` = value of GITHUB_PR_AUTOREVIEW.
    fn run(&self, payload: &serde_json::Value, flag: Option<&str>) -> std::process::Output {
        use std::io::Write;
        let mut cmd = Command::new(github_plugin_dir().join("handlers").join("pr-review.sh"));
        cmd.env("PATH", &self.path_env)
            .env("TMPDIR", &self.dir)
            .env("AISH_STUB_CALLS", &self.calls)
            .env("WEBHOOK_TENANT_ID", "t_fixture")
            .env_remove("GITHUB_PR_AUTOREVIEW")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(v) = flag {
            cmd.env("GITHUB_PR_AUTOREVIEW", v);
        }
        let mut child = cmd.spawn().expect("spawn pr-review.sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "pr-review.sh exit 0: {out:?}");
        out
    }

    /// Lines recorded by the stub, waiting up to `wait` for at least `want`.
    fn calls(&self, want: usize, wait: Duration) -> Vec<String> {
        let deadline = Instant::now() + wait;
        loop {
            let lines: Vec<String> = std::fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            if lines.len() >= want || Instant::now() >= deadline {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn pr_autoreview_off_by_default() {
    let sb = ReviewSandbox::new("ar-off");
    let pr = fixture("pull_request-opened.json");
    sb.run(&pr, None);
    sb.run(&pr, Some("0"));
    sb.run(&pr, Some("true")); // only the exact value "1" enables it
    assert!(sb.calls(1, Duration::from_millis(500)).is_empty());
}

#[test]
fn pr_autoreview_dispatches_once_per_head_sha() {
    let sb = ReviewSandbox::new("ar-on");
    let pr = fixture("pull_request-opened.json");
    let out = sb.run(&pr, Some("1"));
    // Summary line on stdout is unchanged by the dispatch.
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("[github/pr] acme/widgets#42 opened"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("review agent dispatched"));

    let calls = sb.calls(1, Duration::from_secs(5));
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("--coordinator --run-id pr-review-42-ec26c3e-"),
        "{calls:?}"
    );
    assert!(
        calls[0].contains("pull request #42 in repo acme/widgets"),
        "{calls:?}"
    );
    assert!(
        calls[0].contains("ec26c3e57ca3a959ca5aad62de7213c562f8c821"),
        "{calls:?}"
    );

    // Redelivery and ready_for_review on the same head SHA are deduped.
    let out = sb.run(&pr, Some("1"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already dispatched"));
    let mut ready = pr.clone();
    ready["action"] = "ready_for_review".into();
    sb.run(&ready, Some("1"));
    assert_eq!(sb.calls(2, Duration::from_millis(500)).len(), 1);

    // A new head SHA (synchronize → reopened) gets a fresh review.
    let mut pushed = pr.clone();
    pushed["action"] = "reopened".into();
    pushed["pull_request"]["head"]["sha"] = "1234567890abcdef1234567890abcdef12345678".into();
    sb.run(&pushed, Some("1"));
    assert_eq!(sb.calls(2, Duration::from_secs(5)).len(), 2);
}

#[test]
fn pr_autoreview_skips_drafts_and_other_actions() {
    let sb = ReviewSandbox::new("ar-skip");
    let out = sb.run(&fixture("pull_request-draft.json"), Some("1"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("skipped: PR is a draft"));

    let mut closed = fixture("pull_request-opened.json");
    closed["action"] = "closed".into();
    sb.run(&closed, Some("1"));
    assert!(sb.calls(1, Duration::from_millis(500)).is_empty());
}
