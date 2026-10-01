//! Golden + conformance tests for the skill-registry mirror generator (TASK-694).
//!
//! These drive the real `skill-mirror` binary (via `CARGO_BIN_EXE_skill-mirror`)
//! so exit codes and stderr accounting are covered end-to-end, not just the
//! library internals.
//!
//! Coverage map:
//!   * `golden_index_bytes_are_exact`   — fixture tree → byte-exact index.json
//!   * `raw_objects_are_verbatim`       — `{owner}/{name}/raw` == input bytes
//!   * `bad_skills_are_skipped_run_still_succeeds` — the four negative fixtures
//!   * `regeneration_is_byte_identical` — determinism across reruns
//!   * `zero_valid_rows_exits_nonzero` / `min_rows_violation_exits_nonzero`
//!   * `generated_index_is_consumed_by_the_client` — cross-crate conformance
//!
//! No tempfile dependency: fixtures are built under `std::env::temp_dir()` with
//! a pid+nanos-unique name and removed on success (kept on failure for triage).

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "skill-mirror-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("creating temp root");
    dir
}

/// Write `{root}/input/{owner}/{dir}/SKILL.md` with `body`.
fn write_skill(root: &Path, owner: &str, dir: &str, body: &str) {
    let d = root.join("input").join(owner).join(dir);
    std::fs::create_dir_all(&d).expect("creating skill dir");
    std::fs::write(d.join("SKILL.md"), body).expect("writing SKILL.md");
}

/// Write the optional ingest `stars` sidecar next to a skill's SKILL.md.
fn write_stars(root: &Path, owner: &str, dir: &str, stars: u64) {
    let d = root.join("input").join(owner).join(dir);
    std::fs::create_dir_all(&d).expect("creating skill dir");
    std::fs::write(d.join("stars"), stars.to_string()).expect("writing stars");
}

struct Run {
    status: std::process::ExitStatus,
    stderr: String,
}

fn generate(root: &Path, out_name: &str, extra: &[&str]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_skill-mirror"));
    cmd.arg("generate")
        .arg("--input")
        .arg(root.join("input"))
        .arg("--out")
        .arg(root.join(out_name));
    for a in extra {
        cmd.arg(a);
    }
    let output = cmd.output().expect("running skill-mirror");
    Run {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// The three-skill happy-path fixture tree. Note `alpha-skill` vs reference
/// `acme/alpha`: the reference's second segment comes from the frontmatter
/// `name:`, not the directory name.
fn happy_fixture(root: &Path) {
    write_skill(
        root,
        "acme",
        "alpha-skill",
        "---\nname: alpha\ndescription: Alpha does the first thing\nversion: 1.0.0\n---\n\n# Alpha\n\nInstructions.\n",
    );
    write_skill(
        root,
        "acme",
        "beta-skill",
        "---\nname: beta\ndescription: Beta does the second thing\n---\n\n# Beta\n",
    );
    write_skill(
        root,
        "zeta",
        "gamma",
        "---\nname: gamma\ndescription: Gamma is last alphabetically\nversion: 0.2.0\n---\n\n# Gamma\n",
    );
    write_stars(root, "zeta", "gamma", 7);
}

/// Byte-exact expectation for [`happy_fixture`] with `--pretty`: a bare array,
/// sorted by `reference`, declaration-order fields, trailing newline.
const GOLDEN_PRETTY: &str = r#"[
  {
    "name": "alpha",
    "author": "acme",
    "description": "Alpha does the first thing",
    "version": "1.0.0",
    "reference": "acme/alpha",
    "stars": 0
  },
  {
    "name": "beta",
    "author": "acme",
    "description": "Beta does the second thing",
    "version": "",
    "reference": "acme/beta",
    "stars": 0
  },
  {
    "name": "gamma",
    "author": "zeta",
    "description": "Gamma is last alphabetically",
    "version": "0.2.0",
    "reference": "zeta/gamma",
    "stars": 7
  }
]
"#;

// ---------------------------------------------------------------------------
// Golden
// ---------------------------------------------------------------------------

#[test]
fn golden_index_bytes_are_exact() {
    let root = tmp_root("golden");
    happy_fixture(&root);

    let run = generate(&root, "out", &["--pretty"]);
    assert!(run.status.success(), "generate failed: {}", run.stderr);

    let got = std::fs::read_to_string(root.join("out").join("index.json")).expect("index.json");
    assert_eq!(
        got, GOLDEN_PRETTY,
        "index.json bytes drifted from the golden"
    );
    assert!(
        run.stderr.contains("skipped 0 of 3"),
        "expected a 0-of-3 summary, got: {}",
        run.stderr
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn raw_objects_are_verbatim() {
    let root = tmp_root("raw");
    let body = "---\nname: alpha\ndescription: Alpha does the first thing\nversion: 1.0.0\n---\n\n# Alpha\n\nInstructions.\n";
    write_skill(&root, "acme", "alpha-skill", body);

    let run = generate(&root, "out", &[]);
    assert!(run.status.success(), "generate failed: {}", run.stderr);

    // The raw object sits at {owner}/{name}/raw — the exact path the client's
    // `raw_url_on` composes for the ref `acme/alpha`.
    let raw = root.join("out").join("acme").join("alpha").join("raw");
    let got = std::fs::read(&raw).unwrap_or_else(|e| panic!("reading {}: {e}", raw.display()));
    assert_eq!(got, body.as_bytes(), "raw object was not byte-verbatim");

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// Negative fixtures — each skipped, run still exits 0
// ---------------------------------------------------------------------------

#[test]
fn bad_skills_are_skipped_run_still_succeeds() {
    let root = tmp_root("negative");

    // One good skill, so the catalog is non-empty and the run can exit 0.
    write_skill(
        &root,
        "acme",
        "good",
        "---\nname: good\ndescription: The only valid skill here\n---\n\n# Good\n",
    );
    // (1) no frontmatter at all
    write_skill(
        &root,
        "acme",
        "nofront",
        "# Just a heading, no frontmatter\n",
    );
    // (2) path-traversal attempt in the frontmatter name
    write_skill(
        &root,
        "acme",
        "traversal",
        "---\nname: ../../etc/passwd\ndescription: Tries to escape\n---\n\n# Nope\n",
    );
    // (3) over the 256 KiB cap
    let mut huge =
        String::from("---\nname: huge\ndescription: Too big to publish\n---\n\n# Huge\n\n");
    huge.push_str(&"x".repeat(300 * 1024));
    write_skill(&root, "acme", "huge", &huge);
    // (4) duplicate reference — a different directory, same owner + same
    //     frontmatter name, so both resolve to `acme/good`.
    write_skill(
        &root,
        "acme",
        "zz-dupe",
        "---\nname: good\ndescription: A second skill claiming the same reference\n---\n\n# Dupe\n",
    );

    let run = generate(&root, "out", &["--pretty"]);
    assert!(
        run.status.success(),
        "per-file failures must not fail the run; stderr: {}",
        run.stderr
    );

    // All four rejected, exactly one row published.
    for needle in [
        "frontmatter",
        "../../etc/passwd",
        "over the 262144-byte cap",
        "duplicate reference",
    ] {
        assert!(
            run.stderr.contains(needle),
            "expected a WARN mentioning {needle:?}; stderr: {}",
            run.stderr
        );
    }
    assert!(
        run.stderr.contains("skipped 4 of 5"),
        "expected a 4-of-5 summary, got: {}",
        run.stderr
    );

    let index = std::fs::read_to_string(root.join("out").join("index.json")).expect("index.json");
    let rows: serde_json::Value = serde_json::from_str(&index).expect("index.json is valid JSON");
    let arr = rows.as_array().expect("index.json is a bare array");
    assert_eq!(arr.len(), 1, "only the valid skill should be published");
    assert_eq!(arr[0]["reference"], "acme/good");
    assert_eq!(
        arr[0]["description"], "The only valid skill here",
        "the FIRST duplicate must win, deterministically"
    );

    // The rejected traversal name must not have produced any directory.
    assert!(
        !root.join("out").join("acme").join("etc-passwd").exists(),
        "a rejected skill must not leave a raw object behind"
    );

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn regeneration_is_byte_identical() {
    let root = tmp_root("determinism");
    happy_fixture(&root);

    let first = generate(&root, "out-a", &["--pretty"]);
    let second = generate(&root, "out-b", &["--pretty"]);
    assert!(first.status.success() && second.status.success());

    let a = std::fs::read(root.join("out-a").join("index.json")).expect("first index.json");
    let b = std::fs::read(root.join("out-b").join("index.json")).expect("second index.json");
    assert_eq!(
        a, b,
        "two runs over the same tree must emit identical bytes"
    );

    // Compact mode must be stable too.
    let c = generate(&root, "out-c", &[]);
    let d = generate(&root, "out-d", &[]);
    assert!(c.status.success() && d.status.success());
    assert_eq!(
        std::fs::read(root.join("out-c").join("index.json")).unwrap(),
        std::fs::read(root.join("out-d").join("index.json")).unwrap(),
        "compact output must be byte-stable as well"
    );

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// Catalog-level guards
// ---------------------------------------------------------------------------

#[test]
fn zero_valid_rows_exits_nonzero() {
    let root = tmp_root("empty");
    write_skill(&root, "acme", "nofront", "# no frontmatter here\n");

    let run = generate(&root, "out", &[]);
    assert!(
        !run.status.success(),
        "a catalog with zero valid rows must exit non-zero"
    );
    assert!(
        run.stderr.contains("no valid skills found"),
        "stderr: {}",
        run.stderr
    );
    // Nothing may be published when the guard trips.
    assert!(
        !root.join("out").join("index.json").exists(),
        "a failed run must not write index.json"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn min_rows_violation_exits_nonzero() {
    let root = tmp_root("minrows");
    happy_fixture(&root);

    // 3 valid rows, but we demand 10 — the anti-shrink guard must trip.
    let run = generate(&root, "out", &["--min-rows", "10"]);
    assert!(
        !run.status.success(),
        "--min-rows violation must exit non-zero"
    );
    assert!(
        run.stderr.contains("below --min-rows 10"),
        "stderr: {}",
        run.stderr
    );
    assert!(
        !root.join("out").join("index.json").exists(),
        "the guard must trip before any write"
    );

    // ...and the same tree passes when the floor is satisfiable.
    let ok = generate(&root, "out2", &["--min-rows", "3"]);
    assert!(ok.status.success(), "stderr: {}", ok.stderr);

    std::fs::remove_dir_all(&root).ok();
}

// ---------------------------------------------------------------------------
// Cross-crate conformance (links TASK-699)
// ---------------------------------------------------------------------------

/// The generated `index.json` must be consumable by the **client's** read path.
///
/// `skill_provider::search_with_base("file://…")` resolves the URL to a path and
/// then delegates to `aish::skill_contract::search_file_index`, so calling that
/// function here exercises the same in-process code the client runs for a
/// `file://` registry base — without needing a loopback server.
#[test]
fn generated_index_is_consumed_by_the_client() {
    let root = tmp_root("conformance");
    happy_fixture(&root);

    let run = generate(&root, "out", &["--pretty"]);
    assert!(run.status.success(), "generate failed: {}", run.stderr);
    let index = root.join("out").join("index.json");

    // Empty query ⇒ the whole catalog comes back, in emitted order.
    let all = aish::skill_contract::search_file_index(&index, "")
        .expect("client failed to read the generated index");
    let refs: Vec<String> = all.iter().map(|r| r.ref_or_synth()).collect();
    assert_eq!(refs, vec!["acme/alpha", "acme/beta", "zeta/gamma"]);

    // Fields survive the round-trip through the client's own row type.
    assert_eq!(all[0].name, "alpha");
    assert_eq!(all[0].author, "acme");
    assert_eq!(all[0].version, "1.0.0");
    assert_eq!(all[0].description, "Alpha does the first thing");
    assert_eq!(all[2].stars, 7);
    assert_eq!(all[0].short_name(), "acme/alpha");

    // ...and the client's substring filter narrows it, as a real search would.
    let hits = aish::skill_contract::search_file_index(&index, "gamma")
        .expect("client failed to filter the generated index");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].reference, "zeta/gamma");

    std::fs::remove_dir_all(&root).ok();
}
