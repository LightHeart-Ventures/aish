//! End-to-end tests for `skill-mirror ingest` (TASK-695).
//!
//! The crawl is exercised against a loopback HTTP server standing in for both
//! `api.github.com` and `raw.githubusercontent.com` — which is the entire reason
//! both base URLs are constructor arguments on `GitHubClient`. Each test runs
//! the real binary, so argument parsing, the async runtime, concurrency, the
//! write phase, and the exit code are all covered, not just the pure helpers.
//!
//! The server also records every request path, which is how the cost-sensitive
//! claims are *proved* rather than asserted by inspection:
//!
//!   * a 304 means **zero** raw fetches for that repo;
//!   * an oversize blob is skipped from the tree listing, so its bytes are
//!     never requested at all.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// a very small HTTP test server
// ---------------------------------------------------------------------------

struct Req {
    /// Request target, e.g. `/repos/acme/widgets/git/trees/HEAD?recursive=1`.
    target: String,
    /// Lower-cased header names.
    headers: HashMap<String, String>,
}

impl Req {
    /// Target with any query string removed.
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        }
    }
    fn text(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: body.as_bytes().to_vec(),
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

type Handler = dyn Fn(&Req) -> Resp + Send + Sync + 'static;

struct Server {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding a loopback port");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let hits_bg = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (handler, hits) = (handler.clone(), hits_bg.clone());
                // A thread per connection: ingest crawls repos concurrently, so
                // a serial server would serialize (and could deadlock) the test.
                std::thread::spawn(move || serve(stream, &handler, &hits));
            }
        });
        Self { base, hits }
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn count_containing(&self, needle: &str) -> usize {
        self.hits().iter().filter(|h| h.contains(needle)).count()
    }

    fn count_exact(&self, target: &str) -> usize {
        self.hits().iter().filter(|h| *h == target).count()
    }

    fn clear_hits(&self) {
        self.hits.lock().unwrap().clear();
    }
}

fn serve(mut stream: TcpStream, handler: &Arc<Handler>, hits: &Arc<Mutex<Vec<String>>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.lines();
    let target = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    hits.lock().unwrap().push(target.clone());

    let resp = handler(&Req { target, headers });
    let reason = match resp.status {
        200 => "OK",
        304 => "Not Modified",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", resp.status);
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    // A 304 carries no body and (per RFC 9110) no Content-Length.
    if resp.status != 304 {
        head.push_str(&format!("content-length: {}\r\n", resp.body.len()));
    }
    head.push_str("connection: close\r\n\r\n");
    let _ = stream.write_all(head.as_bytes());
    if resp.status != 304 {
        let _ = stream.write_all(&resp.body);
    }
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn skill_md(name: &str, desc: &str) -> String {
    format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n\nBody.\n")
}

/// A `git/trees` response body from `(path, size)` pairs.
fn tree_json(sha: &str, blobs: &[(&str, u64)]) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = blobs
        .iter()
        .map(|(p, size)| {
            serde_json::json!({ "path": p, "type": "blob", "size": size, "mode": "100644" })
        })
        .collect();
    serde_json::json!({ "sha": sha, "truncated": false, "tree": rows })
}

fn repo_json(stars: u64) -> serde_json::Value {
    serde_json::json!({
        "stargazers_count": stars,
        "default_branch": "main",
        "full_name": "fixture/repo",
    })
}

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("skill-mirror-it-{tag}-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
    fn write(&self, rel: &str, body: &str) -> PathBuf {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
        p
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

/// Invoke the real `skill-mirror ingest` binary.
fn ingest(server: &Server, args: &[&str]) -> Run {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_skill-mirror"));
    cmd.arg("ingest")
        .arg("--api-base")
        .arg(&server.base)
        .arg("--raw-base")
        .arg(format!("{}/raw", server.base))
        .args(args)
        // The host's token must not leak into a test that asserts on headers.
        .env_remove("GITHUB_TOKEN");
    let out = cmd.output().expect("running skill-mirror");
    Run {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The happy path: a root SKILL.md and a nested one are both discovered, the
/// output matches the contract `generate` consumes, oversize blobs are skipped
/// without being downloaded, and the token reaches GitHub.
#[test]
fn ingest_writes_the_generate_contract_and_skips_oversize_blobs() {
    let ws = Workspace::new("happy");
    let saw_auth = Arc::new(Mutex::new(Vec::<String>::new()));
    let auth_sink = saw_auth.clone();

    let server = Server::start(move |req| {
        if let Some(a) = req.header("authorization") {
            auth_sink.lock().unwrap().push(a.to_string());
        }
        match req.path() {
            "/repos/acme/widgets" => Resp::json(repo_json(123)),
            "/repos/acme/widgets/git/trees/HEAD" => Resp::json(tree_json(
                "tree-sha-1",
                &[
                    ("README.md", 10),
                    ("SKILL.md", 120),
                    ("skills/huge/SKILL.md", 900_000),
                    ("skills/nested/SKILL.md", 130),
                ],
            ))
            .with_header("etag", "\"etag-one\""),
            "/raw/acme/widgets/main/SKILL.md" => {
                Resp::text(&skill_md("root-skill", "lives at the repo root"))
            }
            "/raw/acme/widgets/main/skills/nested/SKILL.md" => {
                Resp::text(&skill_md("nested-skill", "lives in a subdirectory"))
            }
            _ => Resp::status(404),
        }
    });

    let allow = ws.write("allowlist.toml", "seeds = [\"acme/widgets\"]\n");
    let out = ws.path("out");
    let state = ws.path("state.json");
    let run = ingest(
        &server,
        &[
            "--allowlist",
            allow.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
            "--token",
            "t0ken",
            "--max-size",
            "1000",
        ],
    );
    assert!(run.ok(), "ingest failed: {}\n{}", run.stdout, run.stderr);

    // Both skills landed, keyed by frontmatter `name:`, not by repo path.
    for (dir, name) in [
        ("root-skill", "root-skill"),
        ("nested-skill", "nested-skill"),
    ] {
        let d = out.join("acme").join(dir);
        assert!(read(&d.join("SKILL.md")).contains(&format!("name: {name}")));
        // `generate`'s read_stars parses exactly this sidecar.
        assert_eq!(read(&d.join("stars")).trim(), "123");
        let src: serde_json::Value = serde_json::from_str(&read(&d.join("source.json"))).unwrap();
        assert_eq!(src["repo"], "acme/widgets");
        assert_eq!(src["tree_sha"], "tree-sha-1");
        assert_eq!(src["stars"], 123);
    }

    // The oversize blob cost zero bandwidth: the tree's size field was enough.
    assert_eq!(
        server.count_containing("skills/huge/SKILL.md"),
        0,
        "an oversize SKILL.md must never be fetched; hits: {:?}",
        server.hits()
    );
    assert!(run.stderr.contains("over the 1000-byte cap"));

    // The token was forwarded on every request.
    let auths = saw_auth.lock().unwrap().clone();
    assert!(!auths.is_empty(), "no Authorization header reached GitHub");
    assert!(auths.iter().all(|a| a == "Bearer t0ken"), "got {auths:?}");

    // The ETag was persisted for the next run.
    let st: serde_json::Value = serde_json::from_str(&read(&state)).unwrap();
    assert_eq!(st["repos"]["acme/widgets"]["etag"], "\"etag-one\"");
    assert_eq!(st["repos"]["acme/widgets"]["stars"], 123);
    let cached: Vec<&str> = st["repos"]["acme/widgets"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(cached, vec!["acme/nested-skill", "acme/root-skill"]);
    // The resolved branch is remembered so the next probe skips GET /repos.
    assert_eq!(st["repos"]["acme/widgets"]["git_ref"], "main");
}

/// The cost claim: an unchanged repo answers 304 and the run fetches nothing
/// else. And the cache is trusted only while it is actually on disk — delete the
/// output and the same 304 must trigger a full recrawl.
#[test]
fn a_304_skips_the_repo_but_a_missing_output_forces_a_recrawl() {
    let ws = Workspace::new("etag");
    let server = Server::start(move |req| match req.path() {
        "/repos/acme/widgets" => Resp::json(repo_json(7)),
        // Run 1 probes HEAD; later runs reuse the ref resolved into state (main).
        "/repos/acme/widgets/git/trees/HEAD" | "/repos/acme/widgets/git/trees/main" => {
            // Honour the conditional request, exactly like GitHub.
            if req.header("if-none-match") == Some("\"etag-one\"") {
                return Resp::status(304);
            }
            Resp::json(tree_json("tree-sha-1", &[("SKILL.md", 120)]))
                .with_header("etag", "\"etag-one\"")
        }
        "/raw/acme/widgets/main/SKILL.md" => Resp::text(&skill_md("only-skill", "the only one")),
        _ => Resp::status(404),
    });

    let allow = ws.write("allowlist.toml", "seeds = [\"acme/widgets\"]\n");
    let out = ws.path("out");
    let state = ws.path("state.json");
    let args: Vec<String> = vec![
        "--allowlist".into(),
        allow.to_str().unwrap().into(),
        "--out".into(),
        out.to_str().unwrap().into(),
        "--state".into(),
        state.to_str().unwrap().into(),
    ];
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    let first = ingest(&server, &argv);
    assert!(first.ok(), "{}", first.stderr);
    assert_eq!(server.count_containing("/raw/"), 1);

    // Run 2: nothing changed upstream.
    server.clear_hits();
    let second = ingest(&server, &argv);
    assert!(second.ok(), "{}", second.stderr);
    assert_eq!(
        server.count_containing("/raw/"),
        0,
        "a 304 must not fetch any content; hits: {:?}",
        server.hits()
    );
    assert_eq!(
        server.count_exact("/repos/acme/widgets"),
        0,
        "a 304 must not spend a GET /repos call either; hits: {:?}",
        server.hits()
    );
    assert!(
        second.stdout.contains("1 cached (304)"),
        "stdout: {}\nstderr: {}",
        second.stdout,
        second.stderr
    );
    // The previously published skill is still there.
    assert!(out.join("acme/only-skill/SKILL.md").is_file());

    // Run 3: the cache claims a skill that is no longer on disk. A cache that
    // lies is worse than no cache, so ingest must refetch despite the 304.
    std::fs::remove_dir_all(out.join("acme/only-skill")).unwrap();
    server.clear_hits();
    let third = ingest(&server, &argv);
    assert!(third.ok(), "{}", third.stderr);
    assert!(third.stderr.contains("recrawling"), "{}", third.stderr);
    assert!(out.join("acme/only-skill/SKILL.md").is_file());
    assert_eq!(server.count_containing("/raw/"), 1);

    // And --no-cache ignores a perfectly good ETag on demand.
    server.clear_hits();
    let forced = ingest(&server, &[&argv[..], &["--no-cache"]].concat());
    assert!(forced.ok(), "{}", forced.stderr);
    assert_eq!(server.count_containing("/raw/"), 1);
}

/// Two repos under one owner publishing the same frontmatter `name:` collapse to
/// a single catalog entry, and the more-starred repo wins.
#[test]
fn duplicate_references_collapse_to_the_more_starred_repo() {
    let ws = Workspace::new("dupes");
    let server = Server::start(move |req| match req.path() {
        "/repos/acme/popular" => Resp::json(repo_json(900)),
        "/repos/acme/obscure" => Resp::json(repo_json(2)),
        "/repos/acme/popular/git/trees/HEAD" => {
            Resp::json(tree_json("sha-pop", &[("SKILL.md", 100)]))
        }
        "/repos/acme/obscure/git/trees/HEAD" => {
            Resp::json(tree_json("sha-obs", &[("SKILL.md", 100)]))
        }
        "/raw/acme/popular/main/SKILL.md" => Resp::text(&skill_md("review", "the popular one")),
        "/raw/acme/obscure/main/SKILL.md" => Resp::text(&skill_md("review", "the obscure one")),
        _ => Resp::status(404),
    });

    let allow = ws.write(
        "allowlist.toml",
        "seeds = [\"acme/obscure\", \"acme/popular\"]\n",
    );
    let out = ws.path("out");
    let run = ingest(
        &server,
        &[
            "--allowlist",
            allow.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--state",
            ws.path("state.json").to_str().unwrap(),
        ],
    );
    assert!(run.ok(), "{}", run.stderr);

    let src: serde_json::Value =
        serde_json::from_str(&read(&out.join("acme/review/source.json"))).unwrap();
    assert_eq!(src["repo"], "acme/popular", "more stars must win");
    assert_eq!(src["reference"], "acme/review");
    assert!(run.stderr.contains("duplicate reference acme/review"));
    assert!(
        run.stdout.contains("1 duplicate(s) dropped"),
        "{}",
        run.stdout
    );
}

/// One dead repo is a warning, not a failed run — but a run that produced
/// nothing at all fails loudly rather than publishing an empty catalog.
#[test]
fn a_dead_repo_warns_while_an_empty_catalog_fails() {
    let ws = Workspace::new("partial");
    let server = Server::start(move |req| match req.path() {
        "/repos/acme/good" => Resp::json(repo_json(5)),
        "/repos/acme/good/git/trees/HEAD" => {
            Resp::json(tree_json("sha-good", &[("SKILL.md", 100)]))
        }
        "/raw/acme/good/main/SKILL.md" => Resp::text(&skill_md("survivor", "made it through")),
        // acme/gone is unreachable, and acme/bare has no SKILL.md anywhere.
        "/repos/acme/bare" => Resp::json(repo_json(1)),
        "/repos/acme/bare/git/trees/HEAD" => {
            Resp::json(tree_json("sha-bare", &[("README.md", 10)]))
        }
        _ => Resp::status(404),
    });

    let out = ws.path("out");
    let mixed = ws.write(
        "mixed.toml",
        "seeds = [\"acme/good\", \"acme/gone\", \"acme/bare\"]\n",
    );
    let run = ingest(
        &server,
        &[
            "--allowlist",
            mixed.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--state",
            ws.path("state.json").to_str().unwrap(),
        ],
    );
    assert!(
        run.ok(),
        "a partial failure must still publish: {}",
        run.stderr
    );
    assert!(out.join("acme/survivor/SKILL.md").is_file());
    assert!(run.stderr.contains("WARN acme/gone"), "{}", run.stderr);
    assert!(run.stdout.contains("1 failed"), "{}", run.stdout);

    // Nothing at all ⇒ non-zero, and no catalog is written.
    let nothing = ws.write("nothing.toml", "seeds = [\"acme/bare\"]\n");
    let empty_out = ws.path("empty-out");
    let failed = ingest(
        &server,
        &[
            "--allowlist",
            nothing.to_str().unwrap(),
            "--out",
            empty_out.to_str().unwrap(),
            "--state",
            ws.path("empty-state.json").to_str().unwrap(),
        ],
    );
    assert_eq!(failed.status, Some(1), "{}", failed.stdout);
    assert!(
        failed.stderr.contains("produced no skills"),
        "{}",
        failed.stderr
    );

    // --min-skills is the anti-shrink guard: one skill is not enough.
    let guard = ingest(
        &server,
        &[
            "--allowlist",
            ws.write("one.toml", "seeds = [\"acme/good\"]\n")
                .to_str()
                .unwrap(),
            "--out",
            ws.path("guard-out").to_str().unwrap(),
            "--state",
            ws.path("guard-state.json").to_str().unwrap(),
            "--min-skills",
            "5",
        ],
    );
    assert_eq!(guard.status, Some(1));
    assert!(
        guard.stderr.contains("below --min-skills 5"),
        "{}",
        guard.stderr
    );
}

/// A pinned `ref` and a `path` prefix from the allowlist both reach the wire: the
/// prefix narrows discovery, and the ref is what raw objects are fetched at.
#[test]
fn allowlist_ref_and_path_prefix_are_honoured() {
    let ws = Workspace::new("prefix");
    let server = Server::start(move |req| match req.path() {
        "/repos/acme/mono" => Resp::json(repo_json(11)),
        // The pinned ref is used for the tree probe, not the default branch.
        "/repos/acme/mono/git/trees/v2.0.0" => Resp::json(tree_json(
            "sha-mono",
            &[
                ("SKILL.md", 100),
                ("vendor/copy/SKILL.md", 100),
                ("skills/kept/SKILL.md", 100),
            ],
        )),
        "/raw/acme/mono/v2.0.0/skills/kept/SKILL.md" => {
            Resp::text(&skill_md("kept", "inside the prefix"))
        }
        _ => Resp::status(404),
    });

    let allow = ws.write(
        "allowlist.toml",
        "[[repo]]\nowner = \"acme\"\nrepo = \"mono\"\nref = \"v2.0.0\"\npath = \"skills\"\n",
    );
    let out = ws.path("out");
    let run = ingest(
        &server,
        &[
            "--allowlist",
            allow.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--state",
            ws.path("state.json").to_str().unwrap(),
        ],
    );
    assert!(run.ok(), "{}", run.stderr);
    assert!(out.join("acme/kept/SKILL.md").is_file());
    // Outside the prefix ⇒ never even requested.
    assert_eq!(server.count_containing("vendor/copy/SKILL.md"), 0);
    assert_eq!(server.count_containing("/raw/acme/mono/v2.0.0/SKILL.md"), 0);
    let src: serde_json::Value =
        serde_json::from_str(&read(&out.join("acme/kept/source.json"))).unwrap();
    assert_eq!(src["ref"], "v2.0.0");
}

/// A malformed allowlist fails before a single request is made — the budget is
/// never spent on a document a reviewer typo'd.
#[test]
fn a_bad_allowlist_fails_before_any_network_call() {
    let ws = Workspace::new("badlist");
    let server = Server::start(|_| Resp::status(500));
    let allow = ws.write(
        "allowlist.toml",
        "[[repo]]\nowner = \"acme\"\nbranch = \"main\"\n",
    );
    let run = ingest(
        &server,
        &[
            "--allowlist",
            allow.to_str().unwrap(),
            "--out",
            ws.path("out").to_str().unwrap(),
            "--state",
            ws.path("state.json").to_str().unwrap(),
        ],
    );
    assert_eq!(run.status, Some(1));
    assert!(
        run.stderr.contains("unknown key `branch`"),
        "{}",
        run.stderr
    );
    assert!(server.hits().is_empty(), "hits: {:?}", server.hits());
}
