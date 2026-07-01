//! Integration tests for the plugin webhook dispatcher (Phase 1.6).
//!
//! `aish` is a binary crate with no library target, so an integration test can't
//! `use aish::plugin_dispatcher`. We compile the module's source (and its one
//! sibling dependency, `plugin_state`) directly into this test binary via
//! `#[path]` — inside the module, `crate::plugin_state` then resolves to the
//! sibling module declared at THIS test crate's root.
//!
//! No `wiremock` / `tempfile`: the crate ships no `[dev-dependencies]`, and the
//! established pattern (see `tests/plugin_state_tests.rs`, `src/plugins.rs`) is
//! std-only helpers. The HTTP tests use a tiny one-shot `std::net::TcpListener`
//! mock; the command tests use a hand-rolled temp dir. Regular `[dependencies]`
//! (reqwest, tokio, serde_json) ARE linkable from an integration test.
//!
//! Run with the same gate CI uses:
//!   cargo test --no-default-features --locked plugin_dispatcher

#[path = "../src/plugin_state.rs"]
#[allow(dead_code)]
mod plugin_state;

#[path = "../src/plugin_dispatcher.rs"]
#[allow(dead_code)]
mod plugin_dispatcher;

use plugin_dispatcher::{Event, PluginDispatcher};
use plugin_state::PluginStateStore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A unique, dependency-free temp dir.
fn tempdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "aish-plugin-dispatch-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Write `<root>/<id>/plugin.json` with the given manifest JSON.
fn write_plugin(root: &PathBuf, id: &str, manifest: &str) {
    let pdir = root.join(id);
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join("plugin.json"), manifest).unwrap();
}

/// Spin up a one-shot HTTP/1.1 mock server on a free localhost port. Returns the
/// bound `http://127.0.0.1:PORT/` URL and a receiver that yields the raw request
/// body of the FIRST request received. The accept loop consumes `expect`
/// requests (so a multi-plugin test can drain more than one), replying `200 OK`
/// to each; each request's body is sent on the channel.
fn mock_http_server(expect: usize) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..expect {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let body = read_http_body(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
            let _ = stream.flush();
            let _ = tx.send(body);
        }
    });
    (url, rx)
}

/// Read a single HTTP request off `stream` and return its body. Parses headers
/// to find `Content-Length`, then reads exactly that many body bytes.
fn read_http_body(stream: &mut std::net::TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(v) = trimmed
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    String::from_utf8_lossy(&body).into_owned()
}

/// HTTP dispatch: a plugin with `webhook_url` receives a POST whose JSON body
/// carries the event type + plugin id.
#[tokio::test]
async fn test_route_workspace_open_http() {
    let (url, rx) = mock_http_server(1);
    let dir = tempdir("http");
    write_plugin(
        &dir,
        "webby",
        &format!(r#"{{"id":"webby","webhook_url":"{url}"}}"#),
    );
    let state = PluginStateStore::open_in_memory().unwrap();
    let d = PluginDispatcher::new(dir, state);

    let n = d.route_awaiting(Event::WorkspaceOpen).await.unwrap();
    assert_eq!(n, 1, "exactly one subscriber");

    let body = rx.recv_timeout(Duration::from_secs(5)).expect("server got a POST");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(v["event_type"], "workspace_open");
    assert_eq!(v["plugin_id"], "webby");
    assert!(v["payload_json"].get("cwd").is_some(), "workspace_open carries cwd");
}

/// Command dispatch: a plugin with `webhook_command` runs a shell script; its
/// stdout is captured into plugin state under `<id>:last_webhook_output`, and
/// the event type is exposed on stdin + `$AISH_EVENT_TYPE`.
#[tokio::test]
async fn test_route_skill_loaded_command() {
    let dir = tempdir("cmd");
    // Echo the event type (from env) so we can assert it flowed through.
    write_plugin(
        &dir,
        "scripty",
        r#"{"id":"scripty","webhook_command":"printf handled:$AISH_EVENT_TYPE"}"#,
    );
    let state = PluginStateStore::open_in_memory().unwrap();
    let d = PluginDispatcher::new(dir, state.clone());

    d.route_awaiting(Event::SkillLoaded).await.unwrap();

    let out = state
        .get("scripty", "last_webhook_output")
        .unwrap()
        .expect("command output persisted");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["event"], "skill_loaded");
    assert_eq!(out["stdout"], "handled:skill_loaded");
}

/// The same event fans out to every subscribing plugin.
#[tokio::test]
async fn test_route_multiple_plugins() {
    let (url, rx) = mock_http_server(2);
    let dir = tempdir("multi");
    write_plugin(&dir, "alpha", &format!(r#"{{"id":"alpha","webhook_url":"{url}"}}"#));
    write_plugin(&dir, "bravo", &format!(r#"{{"id":"bravo","webhook_url":"{url}"}}"#));
    // A third plugin with NO webhook must be ignored.
    write_plugin(&dir, "silent", r#"{"id":"silent"}"#);
    let state = PluginStateStore::open_in_memory().unwrap();
    let d = PluginDispatcher::new(dir, state);

    let n = d.route_awaiting(Event::ToolInvoked).await.unwrap();
    assert_eq!(n, 2, "only the two webhook plugins subscribe");

    let mut got = Vec::new();
    for _ in 0..2 {
        let body = rx.recv_timeout(Duration::from_secs(5)).expect("a POST");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        got.push(v["plugin_id"].as_str().unwrap().to_string());
    }
    got.sort();
    assert_eq!(got, vec!["alpha", "bravo"]);
}

/// `route()` (fire-and-forget) returns immediately even when the delivery is
/// slow, and the event still completes asynchronously afterward.
#[tokio::test]
async fn test_non_blocking() {
    let dir = tempdir("nonblock");
    // A command that sleeps before producing output — delivery takes ~300ms.
    write_plugin(
        &dir,
        "slow",
        r#"{"id":"slow","webhook_command":"sleep 0.3; printf done"}"#,
    );
    let state = PluginStateStore::open_in_memory().unwrap();
    let d = PluginDispatcher::new(dir, state.clone());

    let start = Instant::now();
    d.route(Event::BackgroundJobStart).unwrap();
    let elapsed = start.elapsed();
    // route() must not wait for the 300ms command.
    assert!(
        elapsed < Duration::from_millis(150),
        "route() should return immediately, took {elapsed:?}"
    );

    // The output is absent right away, then appears once the async task finishes.
    assert!(
        state.get("slow", "last_webhook_output").unwrap().is_none(),
        "delivery has not completed synchronously"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(out) = state.get("slow", "last_webhook_output").unwrap() {
            assert_eq!(out["stdout"], "done");
            break;
        }
        assert!(Instant::now() < deadline, "async delivery never completed");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
