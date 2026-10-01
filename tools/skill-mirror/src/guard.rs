//! `skill-mirror guard` — the anti-shrink publish gate (TASK-698).
//!
//! The nightly refresh job's one unforgivable failure mode is publishing a
//! gutted catalog over a good one: GitHub has a bad night, the crawl comes back
//! with three skills instead of three hundred, and the live registry is wiped
//! by a job that thought it succeeded. `generate --min-rows` is a *static*
//! floor, which is either immediately stale (the catalog grows) or set so low
//! it never fires. This guard is the dynamic half: it compares the freshly
//! generated row count against the count **currently live** and refuses the
//! publish when the delta looks like breakage rather than churn.
//!
//! | new vs live | verdict |
//! |---|---|
//! | ≥ 90 %  | publish |
//! | 50–90 % | blocked, overridable with `--force` from a manual dispatch |
//! | < 50 %  | blocked, never overridable |
//! | live 404 (no catalog yet) | publish — bootstrap |
//! | live unreachable / unparseable | blocked, fail closed |
//!
//! The 404-vs-unreachable split is the load-bearing distinction. A 404 is a
//! *definitive* answer ("there is nothing published") and makes the first run
//! possible. A timeout, a 5xx, or a body that will not parse is an *unknown*,
//! and an unknown must never authorize a publish: we cannot verify we are not
//! about to destroy the catalog, so we don't. `--force` deliberately does not
//! override that, because the thing it would override is precisely our
//! inability to see.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Ratio at or above which a publish is unremarkable.
const HEALTHY: f64 = 0.90;
/// Ratio below which a publish is never allowed, `--force` or not.
const CATASTROPHIC: f64 = 0.50;

#[derive(clap::Args)]
pub struct GuardArgs {
    /// The freshly generated `index.json` whose row count is being gated.
    #[arg(long)]
    pub index: PathBuf,
    /// URL of the **live** `index.json` to compare against (the catalog this
    /// run would replace). Omit only when there is demonstrably nothing live.
    #[arg(long)]
    pub live_url: Option<String>,
    /// Override a soft (50–90 %) block. Intended for `workflow_dispatch` when a
    /// human has looked at the delta and knows the shrink is legitimate (e.g. a
    /// repo was deliberately removed from the allowlist). Has no effect on a
    /// catastrophic (< 50 %) block or on a fail-closed unknown.
    #[arg(long)]
    pub force: bool,
    /// Absolute floor, independent of the live count. Belt and braces for the
    /// bootstrap case where there is no live catalog to form a ratio against.
    #[arg(long, default_value_t = 1)]
    pub min_rows: usize,
    /// Timeout for fetching the live index.
    #[arg(long, default_value_t = 30)]
    pub timeout_secs: u64,
}

/// What we managed to learn about the live catalog. The three states are
/// deliberately distinct: `Absent` authorizes a bootstrap publish, `Unknown`
/// never authorizes anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    /// The live index was fetched and parsed; this many rows are published.
    Known(usize),
    /// The live index is definitively not there (404). First run.
    Absent,
    /// We could not find out. Carries the reason for the log.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Publish. Carries the human-readable reason for the job summary.
    Publish(String),
    /// Do not publish. `overridable` reports whether `--force` *would* have
    /// let it through, so the error message can say so.
    Block { reason: String, overridable: bool },
}

impl Verdict {
    /// Test-only: `run` matches on the variants directly, so this exists
    /// purely to keep the band assertions in the unit tests readable.
    #[cfg(test)]
    pub fn is_publish(&self) -> bool {
        matches!(self, Verdict::Publish(_))
    }
}

/// The whole decision, as a pure function of three inputs — no I/O, no clock,
/// no environment. Every band in the table above is a unit test.
pub fn evaluate(new_rows: usize, live: &Live, force: bool, min_rows: usize) -> Verdict {
    // The absolute floor is checked first and applies in every state: a
    // zero-row catalog is never publishable, however healthy the ratio maths
    // would look against a zero-row live index.
    if new_rows < min_rows.max(1) {
        return Verdict::Block {
            reason: format!(
                "{new_rows} row(s) is below the absolute floor of {}",
                min_rows.max(1)
            ),
            overridable: false,
        };
    }

    match live {
        Live::Absent => Verdict::Publish(format!(
            "no live catalog to compare against (404) — bootstrapping with {new_rows} row(s)"
        )),

        Live::Unknown(why) => Verdict::Block {
            reason: format!(
                "could not read the live catalog ({why}) — failing closed rather than risk \
                 publishing over it unverified"
            ),
            overridable: false,
        },

        // A live catalog that is itself empty cannot be shrunk; anything is an
        // improvement, so the ratio is meaningless and we publish.
        Live::Known(0) => Verdict::Publish(format!(
            "live catalog is empty — publishing {new_rows} row(s)"
        )),

        Live::Known(live_rows) => {
            let ratio = new_rows as f64 / *live_rows as f64;
            let pct = ratio * 100.0;
            if ratio >= HEALTHY {
                Verdict::Publish(format!(
                    "{new_rows} row(s) vs {live_rows} live ({pct:.1}% — healthy)"
                ))
            } else if ratio >= CATASTROPHIC {
                let reason = format!(
                    "{new_rows} row(s) vs {live_rows} live ({pct:.1}%) is a suspicious shrink"
                );
                if force {
                    Verdict::Publish(format!("{reason} — overridden by --force"))
                } else {
                    Verdict::Block {
                        reason,
                        overridable: true,
                    }
                }
            } else {
                Verdict::Block {
                    reason: format!(
                        "{new_rows} row(s) vs {live_rows} live ({pct:.1}%) is a catastrophic \
                         shrink — this is never auto-publishable"
                    ),
                    overridable: false,
                }
            }
        }
    }
}

/// Count the rows in a generated `index.json`. The artifact contract is a bare
/// JSON array (see `emit.rs`), so anything else is a corrupt artifact, not a
/// small one — hence an error rather than a count of zero.
pub fn count_rows(path: &Path) -> Result<usize> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading the generated index at {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    match v {
        serde_json::Value::Array(rows) => Ok(rows.len()),
        other => anyhow::bail!(
            "{} is {}, not the expected JSON array of rows",
            path.display(),
            kind_of(&other)
        ),
    }
}

fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Fetch and count the live catalog. Every failure mode collapses into
/// `Live::Unknown` with the reason preserved — the caller decides what an
/// unknown means (it means: do not publish).
pub async fn fetch_live(url: &str, timeout: Duration) -> Live {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return Live::Unknown(format!("building the http client: {e}")),
    };
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Live::Unknown(format!("GET {url}: {e}")),
    };
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Live::Absent;
    }
    if !resp.status().is_success() {
        return Live::Unknown(format!("GET {url}: HTTP {}", resp.status().as_u16()));
    }
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Live::Unknown(format!("reading the body of {url}: {e}")),
    };
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(serde_json::Value::Array(rows)) => Live::Known(rows.len()),
        Ok(_) => Live::Unknown(format!("{url} is not a JSON array")),
        Err(e) => Live::Unknown(format!("parsing {url}: {e}")),
    }
}

pub async fn run(args: GuardArgs) -> Result<ExitCode> {
    let new_rows = count_rows(&args.index)?;

    let live = match &args.live_url {
        Some(url) => fetch_live(url, Duration::from_secs(args.timeout_secs)).await,
        // No URL supplied is an explicit operator claim that nothing is live.
        // Distinct from a failed fetch, and the only way to publish without a
        // reachable live index.
        None => Live::Absent,
    };

    match &live {
        Live::Known(n) => eprintln!("skill-mirror: live catalog has {n} row(s)"),
        Live::Absent => eprintln!("skill-mirror: no live catalog (bootstrap)"),
        Live::Unknown(why) => eprintln!("skill-mirror: live catalog unreadable: {why}"),
    }

    match evaluate(new_rows, &live, args.force, args.min_rows) {
        Verdict::Publish(why) => {
            println!("guard: PASS — {why}");
            Ok(ExitCode::SUCCESS)
        }
        Verdict::Block {
            reason,
            overridable,
        } => {
            eprintln!("skill-mirror: error: publish BLOCKED — {reason}");
            if overridable {
                eprintln!(
                    "skill-mirror: re-run via workflow_dispatch with force=true if this shrink \
                     is intentional"
                );
            }
            println!("guard: BLOCKED — {reason}");
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(n: usize) -> Live {
        Live::Known(n)
    }

    #[test]
    fn healthy_band_publishes() {
        // Growth, parity, and the exact 90% boundary all pass.
        for new in [300, 100, 90] {
            let v = evaluate(new, &known(100), false, 1);
            assert!(v.is_publish(), "{new} vs 100 should publish, got {v:?}");
        }
    }

    #[test]
    fn suspicious_band_blocks_but_is_overridable() {
        for new in [89, 70, 50] {
            match evaluate(new, &known(100), false, 1) {
                Verdict::Block { overridable, .. } => {
                    assert!(overridable, "{new} vs 100 should be overridable")
                }
                other => panic!("{new} vs 100 should block, got {other:?}"),
            }
            assert!(
                evaluate(new, &known(100), true, 1).is_publish(),
                "--force should carry {new} vs 100 through"
            );
        }
    }

    #[test]
    fn catastrophic_band_is_never_overridable() {
        for new in [49, 10, 1] {
            match evaluate(new, &known(100), true, 1) {
                Verdict::Block { overridable, .. } => assert!(
                    !overridable,
                    "{new} vs 100 must not be overridable even with --force"
                ),
                other => panic!("{new} vs 100 must block even with --force, got {other:?}"),
            }
        }
    }

    #[test]
    fn band_boundaries_are_exact() {
        // The two thresholds are inclusive-from-below: 90% is healthy, 50% is
        // merely suspicious. Pinned because an off-by-one here is the
        // difference between a blocked publish and a wiped catalog.
        assert!(evaluate(900, &known(1000), false, 1).is_publish());
        assert!(!evaluate(899, &known(1000), false, 1).is_publish());
        assert!(evaluate(500, &known(1000), true, 1).is_publish());
        match evaluate(499, &known(1000), true, 1) {
            Verdict::Block { overridable, .. } => assert!(!overridable),
            other => panic!("499/1000 must hard-block, got {other:?}"),
        }
    }

    #[test]
    fn absent_live_catalog_bootstraps() {
        assert!(evaluate(1, &Live::Absent, false, 1).is_publish());
        assert!(evaluate(500, &Live::Absent, false, 1).is_publish());
    }

    #[test]
    fn unknown_live_catalog_fails_closed_even_with_force() {
        let live = Live::Unknown("timeout".into());
        for force in [false, true] {
            match evaluate(1_000_000, &live, force, 1) {
                Verdict::Block { overridable, .. } => assert!(
                    !overridable,
                    "an unverifiable live catalog must never be overridable"
                ),
                other => panic!("unknown live must block (force={force}), got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_new_catalog_is_blocked_in_every_live_state() {
        for live in [Live::Absent, known(0), known(10), Live::Unknown("x".into())] {
            match evaluate(0, &live, true, 1) {
                Verdict::Block { overridable, .. } => assert!(!overridable),
                other => panic!("0 rows must block for {live:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_live_catalog_cannot_be_shrunk() {
        // Ratio maths against zero is undefined; publishing over nothing is
        // always an improvement.
        assert!(evaluate(1, &known(0), false, 1).is_publish());
    }

    #[test]
    fn absolute_floor_outranks_a_healthy_ratio() {
        // 10 vs 10 live is a perfect 100%, but a floor of 50 still rejects it:
        // the floor encodes "we know this catalog should be bigger than this".
        match evaluate(10, &known(10), true, 50) {
            Verdict::Block {
                reason,
                overridable,
            } => {
                assert!(reason.contains("absolute floor"), "reason: {reason}");
                assert!(!overridable);
            }
            other => panic!("the floor must win, got {other:?}"),
        }
    }

    #[test]
    fn count_rows_reads_an_array_and_rejects_other_shapes() {
        let dir = std::env::temp_dir().join(format!("sm-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.json");
        std::fs::write(&good, br#"[{"reference":"a/b"},{"reference":"c/d"}]"#).unwrap();
        assert_eq!(count_rows(&good).unwrap(), 2);

        let empty = dir.join("empty.json");
        std::fs::write(&empty, b"[]").unwrap();
        assert_eq!(count_rows(&empty).unwrap(), 0);

        // An object is a corrupt artifact, not an empty one — erroring beats
        // silently reporting zero rows into a guard that then hard-blocks for
        // the wrong reason.
        let obj = dir.join("obj.json");
        std::fs::write(&obj, br#"{"rows":[]}"#).unwrap();
        let err = count_rows(&obj).unwrap_err().to_string();
        assert!(err.contains("not the expected JSON array"), "err: {err}");

        let junk = dir.join("junk.json");
        std::fs::write(&junk, b"not json").unwrap();
        assert!(count_rows(&junk).is_err());

        assert!(count_rows(&dir.join("absent.json")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
