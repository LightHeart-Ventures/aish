//! End-to-end tests for `skill-mirror guard` (TASK-698).
//!
//! `src/guard.rs` unit-tests the decision function; these drive the real binary
//! against a real socket, because the parts that cannot be unit-tested are
//! exactly the parts that decide whether a catalog survives the night: does an
//! unreachable live index actually fail closed, does the exit code actually
//! stop a workflow, does `--force` actually stop at the catastrophic band.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// a one-route loopback server
// ---------------------------------------------------------------------------

struct Server {
    base: String,
}

impl Server {
    /// Serve `status` + `body` at every path, forever.
    fn start(status: u16, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding a loopback port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let body = body.clone();
                std::thread::spawn(move || respond(stream, status, &body));
            }
        });
        Self { base }
    }

    fn index_url(&self) -> String {
        format!("{}/index.json", self.base)
    }
}

fn respond(mut stream: TcpStream, status: u16, body: &str) {
    let mut buf = [0u8; 2048];
    // Drain the request line/headers; we serve one route, so we don't parse it.
    let _ = stream.read(&mut buf);
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// Bind a port, learn its number, drop the listener. Nothing is listening
/// there now — the closest thing to "the live catalog is unreachable" that a
/// hermetic test can produce without waiting out a real timeout.
fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding a loopback port");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/index.json")
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "skill-mirror-guard-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("creating the workspace");
        Self { root }
    }

    /// Write an `index.json` with `rows` syntactically valid rows.
    fn index(&self, rows: usize) -> PathBuf {
        let rows: Vec<serde_json::Value> = (0..rows)
            .map(|i| serde_json::json!({ "reference": format!("owner/skill-{i}") }))
            .collect();
        let path = self.root.join("index.json");
        std::fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

fn guard(index: &Path, live_url: Option<&str>, extra: &[&str]) -> Run {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_skill-mirror"));
    cmd.arg("guard").arg("--index").arg(index);
    if let Some(url) = live_url {
        cmd.arg("--live-url").arg(url);
    }
    // Keep the fail-closed test fast: a refused connection is immediate, but
    // the timeout is the backstop if the environment black-holes instead.
    cmd.args(["--timeout-secs", "5"]).args(extra);
    let out = cmd.output().expect("running skill-mirror guard");
    Run {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

/// A live `index.json` body with `n` rows.
fn live_body(n: usize) -> String {
    let rows: Vec<serde_json::Value> = (0..n)
        .map(|i| serde_json::json!({ "reference": format!("owner/live-{i}") }))
        .collect();
    serde_json::to_string(&rows).unwrap()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn a_healthy_catalog_publishes() {
    let ws = Workspace::new("healthy");
    let server = Server::start(200, live_body(100));
    let run = guard(&ws.index(104), Some(&server.index_url()), &[]);
    assert!(
        run.ok(),
        "expected exit 0, got {:?}: {}",
        run.status,
        run.stderr
    );
    assert!(run.stdout.contains("guard: PASS"), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("live catalog has 100 row(s)"),
        "the live count belongs in the log: {}",
        run.stderr
    );
}

#[test]
fn a_suspicious_shrink_blocks_with_a_nonzero_exit() {
    let ws = Workspace::new("soft");
    let server = Server::start(200, live_body(100));
    let run = guard(&ws.index(70), Some(&server.index_url()), &[]);
    assert_eq!(run.status, Some(1), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("guard: BLOCKED"),
        "stdout: {}",
        run.stdout
    );
    // The operator needs to be told the override exists, or they will reach
    // for something worse (editing the allowlist, deleting the guard).
    assert!(
        run.stderr.contains("force=true"),
        "a soft block must advertise the override: {}",
        run.stderr
    );
}

#[test]
fn force_carries_a_suspicious_shrink_through() {
    let ws = Workspace::new("soft-forced");
    let server = Server::start(200, live_body(100));
    let run = guard(&ws.index(70), Some(&server.index_url()), &["--force"]);
    assert!(
        run.ok(),
        "expected exit 0, got {:?}: {}",
        run.status,
        run.stderr
    );
    assert!(
        run.stdout.contains("overridden by --force"),
        "an override must be visible in the verdict: {}",
        run.stdout
    );
}

#[test]
fn force_cannot_carry_a_catastrophic_shrink_through() {
    let ws = Workspace::new("hard");
    let server = Server::start(200, live_body(100));
    let run = guard(&ws.index(3), Some(&server.index_url()), &["--force"]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("catastrophic"),
        "stderr should name the band: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("force=true"),
        "a hard block must NOT advertise an override that does not exist: {}",
        run.stderr
    );
}

#[test]
fn a_missing_live_catalog_bootstraps() {
    let ws = Workspace::new("bootstrap");
    let server = Server::start(404, String::new());
    let run = guard(&ws.index(12), Some(&server.index_url()), &[]);
    assert!(
        run.ok(),
        "404 is a definitive 'nothing published': {}",
        run.stderr
    );
    assert!(
        run.stdout.contains("bootstrapping"),
        "stdout: {}",
        run.stdout
    );
}

#[test]
fn an_unreachable_live_catalog_fails_closed_even_with_force() {
    let ws = Workspace::new("unreachable");
    let url = dead_url();
    // Even an enormous, obviously-healthy new catalog must not publish: the
    // point is that we cannot see what we would be overwriting.
    let run = guard(&ws.index(10_000), Some(&url), &["--force"]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("failing closed"),
        "stderr should explain the fail-closed: {}",
        run.stderr
    );
}

#[test]
fn a_five_hundred_from_the_live_catalog_also_fails_closed() {
    let ws = Workspace::new("five-hundred");
    let server = Server::start(500, "upstream exploded".into());
    let run = guard(&ws.index(500), Some(&server.index_url()), &["--force"]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(run.stderr.contains("HTTP 500"), "stderr: {}", run.stderr);
}

#[test]
fn an_unparseable_live_catalog_fails_closed() {
    let ws = Workspace::new("garbage");
    let server = Server::start(200, "<html>not the index</html>".into());
    let run = guard(&ws.index(500), Some(&server.index_url()), &[]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(run.stderr.contains("unreadable"), "stderr: {}", run.stderr);
}

#[test]
fn an_empty_generated_catalog_never_publishes() {
    let ws = Workspace::new("empty");
    let server = Server::start(404, String::new());
    // Not even in the bootstrap case, where there is nothing to lose: an empty
    // catalog is a broken crawl, and publishing it teaches the mirror to serve
    // nothing.
    let run = guard(&ws.index(0), Some(&server.index_url()), &["--force"]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("absolute floor"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn the_absolute_floor_is_enforced_during_bootstrap() {
    let ws = Workspace::new("floor");
    let server = Server::start(404, String::new());
    let run = guard(
        &ws.index(5),
        Some(&server.index_url()),
        &["--min-rows", "50", "--force"],
    );
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("below the absolute floor"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn omitting_the_live_url_is_an_explicit_bootstrap() {
    let ws = Workspace::new("no-url");
    let run = guard(&ws.index(7), None, &[]);
    assert!(run.ok(), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("bootstrap"), "stderr: {}", run.stderr);
}

#[test]
fn a_corrupt_generated_index_is_an_error_not_a_zero_count() {
    let ws = Workspace::new("corrupt");
    let path = ws.root.join("index.json");
    std::fs::write(&path, b"{\"rows\": []}").unwrap();
    let run = guard(&path, None, &[]);
    assert_eq!(run.status, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains("not the expected JSON array"),
        "stderr: {}",
        run.stderr
    );
}
