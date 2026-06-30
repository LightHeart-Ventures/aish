//! Resolve a hardware-detected (or operator-pinned) local-model *selection* into
//! a concrete on-disk GGUF file — downloading it from Hugging Face on first use.
//!
//! [`crate::hwdetect`] decides *which* model to run (a `model_id` + an `hf_repo`
//! + a `quant`), but it never fetches anything: it only records the choice in
//! `~/.aish/local-model.json`. Before this module existed, the `local` backend
//! would fall back to opening a bare `"{model_id}.gguf"` in the cwd, which never
//! exists — so `:backend local` always failed with
//! `gguf_init_from_file: failed to open GGUF file ... (No such file or directory)`.
//!
//! [`ensure_model_file`] closes that gap. It maps the selection to a real
//! `.gguf` path under `~/.aish/models/<repo_slug>/`, downloading the weights
//! from `hf_repo` (streamed to a `.part` then atomically renamed) when they are
//! not already cached, handling multi-shard (`-NNNNN-of-MMMMM.gguf`) models, and
//! persisting the resolved absolute path back into the selection so every later
//! launch is a clean, download-free pin.
//!
//! Like [`crate::hwdetect`], this module is feature-independent: it compiles in
//! the default (Claude-only) build so the network/path logic is type-checked and
//! unit-tested by CI, even though it is only *called* from the `local`-feature
//! backend (`crate::backend::local`). Hence the module-level `dead_code` allow —
//! the download/resolve entrypoints have no caller in a non-`local` build.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use crate::hwdetect::{Selection, SelectionSource};

/// Default quantization when a selection records none (matches the curated
/// ladder in [`crate::hwdetect`]).
const DEFAULT_QUANT: &str = "Q4_K_M";

// ---------------------------------------------------------------------------
// Public entrypoint
// ---------------------------------------------------------------------------

/// Resolve `sel` to a concrete, on-disk GGUF path, downloading from Hugging Face
/// when necessary.
///
/// Resolution order:
/// 1. The selection's recorded `model_path`, if it points at an existing file
///    (the fast path on every launch after the first download).
/// 2. Otherwise: derive `(hf_repo, quant)` (from the selection, falling back to
///    the tier table keyed by `model_id`), list the repo's GGUF files via the HF
///    API (falling back to the conventional filename when offline), download any
///    missing shard(s) into `~/.aish/models/<repo_slug>/`, and return the
///    primary file. The resolved path is persisted back into the selection.
pub async fn ensure_model_file(sel: Option<&Selection>) -> Result<PathBuf> {
    // (1) Fast path — a previously-resolved, still-present file.
    if let Some(p) = sel
        .and_then(|s| s.model_path.as_deref())
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Ok(pb);
        }
    }

    let model_id = sel
        .map(|s| s.model_id.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::hwdetect::DEFAULT_MODEL_ID.to_string());

    let (repo, quant) = resolve_repo_quant(sel, &model_id)?;

    let cache_dir = models_dir().join(repo_slug(&repo));
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating model cache dir {}", cache_dir.display()))?;

    let client = build_client()?;
    let base = hf_base();
    let revision = hf_revision();

    // Decide which file(s) make up the model. Prefer the live HF file listing
    // (handles real filenames + sharded models); fall back to the conventional
    // single-file name when the API is unreachable (offline / rate-limited).
    let files: Vec<String> = match fetch_siblings(&client, &base, &repo).await {
        Ok(listing) => {
            let picked = pick_gguf_files(&listing, &quant);
            if picked.is_empty() {
                eprintln!(
                    "\x1b[2m  no '{quant}' GGUF found in {repo} listing; trying conventional name\x1b[0m"
                );
                vec![convention_filename(&repo, &quant)]
            } else {
                picked
            }
        }
        Err(e) => {
            eprintln!("\x1b[2m  could not list {repo} files ({e}); trying conventional name\x1b[0m");
            vec![convention_filename(&repo, &quant)]
        }
    };

    eprintln!(
        "\x1b[2m  resolving local model {model_id} → {} file(s) from {repo}\x1b[0m",
        files.len()
    );

    let mut local_paths: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (i, rfile) in files.iter().enumerate() {
        let basename = basename_of(rfile);
        let dest = cache_dir.join(basename);
        if file_ready(&dest) {
            eprintln!("\x1b[2m  cached: {}\x1b[0m", dest.display());
        } else {
            let url = resolve_url(&base, &repo, &revision, rfile);
            check_url(&url)?;
            let label = if files.len() > 1 {
                format!("{basename} [{}/{}]", i + 1, files.len())
            } else {
                basename.to_string()
            };
            download_file(&client, &url, &dest, &label)
                .await
                .with_context(|| format!("downloading {basename} from {repo}"))?;
        }
        local_paths.push(dest);
    }

    let primary = local_paths
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no GGUF file resolved for local model '{model_id}' (repo {repo})"))?;

    persist_model_path(sel, &primary);
    Ok(primary)
}

// ---------------------------------------------------------------------------
// Resolution helpers (pure)
// ---------------------------------------------------------------------------

/// Derive `(hf_repo, quant)` for a selection. Prefers the repo recorded on the
/// selection, otherwise looks the `model_id` up in the curated tier table.
fn resolve_repo_quant(sel: Option<&Selection>, model_id: &str) -> Result<(String, String)> {
    if let Some(s) = sel {
        let repo = s.hf_repo.trim();
        if !repo.is_empty() {
            let quant = if s.quant.trim().is_empty() {
                DEFAULT_QUANT.to_string()
            } else {
                s.quant.trim().to_string()
            };
            return Ok((repo.to_string(), quant));
        }
    }
    if let Some((repo, quant)) = crate::hwdetect::repo_and_quant_for(model_id) {
        return Ok((repo, quant));
    }
    Err(anyhow!(
        "no Hugging Face repo is known for local model '{model_id}'. Download a GGUF \
         manually and pin it with AISH_LOCAL_MODEL_PATH=/abs/path/model.gguf"
    ))
}

/// `~/.aish/models` — the on-disk GGUF cache root.
fn models_dir() -> PathBuf {
    crate::hwdetect::aish_dir().join("models")
}

/// Filesystem-safe per-repo subdirectory (`owner/Name-GGUF` → `owner_Name-GGUF`),
/// keeping a repo's shards together and avoiding cross-repo basename collisions.
fn repo_slug(repo: &str) -> String {
    repo.replace('/', "_")
}

/// The last path segment of an `rfilename` (which may include a subdirectory for
/// sharded repos, e.g. `Q4_K_M/Model-00001-of-00002.gguf`).
fn basename_of(rfilename: &str) -> &str {
    rfilename.rsplit('/').next().unwrap_or(rfilename)
}

/// Conventional bartowski-style filename for a repo + quant, used only as an
/// offline fallback: `bartowski/Qwen2.5-14B-Instruct-GGUF` + `Q4_K_M`
/// → `Qwen2.5-14B-Instruct-Q4_K_M.gguf`.
fn convention_filename(repo: &str, quant: &str) -> String {
    let last = repo.rsplit('/').next().unwrap_or(repo);
    let stem = last
        .strip_suffix("-GGUF")
        .or_else(|| last.strip_suffix("-gguf"))
        .unwrap_or(last);
    let q = if quant.trim().is_empty() {
        DEFAULT_QUANT
    } else {
        quant.trim()
    };
    format!("{stem}-{q}.gguf")
}

/// Does a basename's quant token match `quant` at a `-`-delimited boundary?
/// Prevents `Q4_K_M` from matching `Q4_K_M_L` while still matching both
/// `…-Q4_K_M.gguf` and the sharded `…-Q4_K_M-00001-of-00002.gguf`.
fn quant_matches(base_lower: &str, quant_lower: &str) -> bool {
    if quant_lower.is_empty() {
        return true;
    }
    let needle = format!("-{quant_lower}");
    if let Some(pos) = base_lower.find(&needle) {
        let after = &base_lower[pos + needle.len()..];
        return after.starts_with('.') || after.starts_with('-');
    }
    false
}

/// From a repo's full file listing, pick the GGUF file(s) for `quant`. Returns
/// all shards of a sharded model (sorted), or the single matching file. Excludes
/// vision projectors (`mmproj*`).
fn pick_gguf_files(files: &[String], quant: &str) -> Vec<String> {
    let q = quant.trim().to_ascii_lowercase();
    let mut matched: Vec<String> = files
        .iter()
        .filter(|f| {
            let base = basename_of(f).to_ascii_lowercase();
            base.ends_with(".gguf") && !base.starts_with("mmproj") && quant_matches(&base, &q)
        })
        .cloned()
        .collect();

    let has_shards = matched
        .iter()
        .any(|f| basename_of(f).to_ascii_lowercase().contains("-of-"));
    if has_shards {
        matched.retain(|f| basename_of(f).to_ascii_lowercase().contains("-of-"));
    }
    matched.sort();
    matched
}

/// Parse `rfilename`s out of an HF `/api/models/{repo}` JSON response.
fn parse_siblings(body: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("siblings")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            e.get("rfilename").and_then(|r| r.as_str()).map(String::from)
                        })
                        .collect::<Vec<_>>()
                })
        })
        .unwrap_or_default()
}

/// HF API model-info URL.
fn api_url(base: &str, repo: &str) -> String {
    format!("{}/api/models/{}", base.trim_end_matches('/'), repo)
}

/// HF file-download URL (`?download=true` asks for the raw bytes, not the
/// HTML wrapper).
fn resolve_url(base: &str, repo: &str, revision: &str, rfilename: &str) -> String {
    format!(
        "{}/{}/resolve/{}/{}?download=true",
        base.trim_end_matches('/'),
        repo,
        revision,
        rfilename
    )
}

/// Guard against fetching from anything but HTTPS — except http loopback, which
/// the unit tests use with a local server.
fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if host == "127.0.0.1" || host == "localhost" || host == "[::1]" {
            return Ok(());
        }
    }
    Err(anyhow!("refusing to fetch local model from non-https URL: {url}"))
}

// ---------------------------------------------------------------------------
// Environment knobs
// ---------------------------------------------------------------------------

/// Base Hugging Face host. Overridable via `AISH_HF_BASE` (used by tests).
fn hf_base() -> String {
    std::env::var("AISH_HF_BASE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string())
}

/// Git revision to resolve files against. Overridable via `AISH_HF_REVISION`.
fn hf_revision() -> String {
    std::env::var("AISH_HF_REVISION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Optional bearer token for gated/private repos (`HF_TOKEN` or
/// `HUGGING_FACE_HUB_TOKEN`).
fn hf_token() -> Option<String> {
    for k in ["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Network + IO
// ---------------------------------------------------------------------------

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aish/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client for model download")
}

/// List a repo's files via the HF API.
async fn fetch_siblings(client: &reqwest::Client, base: &str, repo: &str) -> Result<Vec<String>> {
    let url = api_url(base, repo);
    check_url(&url)?;
    let mut req = client.get(&url);
    if let Some(tok) = hf_token() {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let body = resp.text().await.context("reading HF model-info body")?;
    Ok(parse_siblings(&body))
}

/// Stream a single file to `dest`, via a `.part` temp that is atomically renamed
/// on success, with a TTY progress line on stderr.
async fn download_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<()> {
    let mut req = client.get(url);
    if let Some(tok) = hf_token() {
        req = req.bearer_auth(tok);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;

    let total = resp.content_length();
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);

    let file = std::fs::File::create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut writer = std::io::BufWriter::new(file);

    let tty = is_stderr_tty();
    eprintln!("\x1b[2m  downloading {label}…\x1b[0m");

    let mut downloaded: u64 = 0;
    let mut last_print = Instant::now();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.context("reading response body")? {
        writer
            .write_all(chunk.as_ref())
            .context("writing model file")?;
        downloaded += chunk.len() as u64;
        if tty && last_print.elapsed() >= Duration::from_millis(200) {
            print_progress(downloaded, total);
            last_print = Instant::now();
        }
    }
    writer.flush().context("flushing model file")?;
    drop(writer);
    if tty {
        print_progress(downloaded, total);
        eprintln!();
    }

    std::fs::rename(&tmp, dest)
        .with_context(|| format!("finalizing {}", dest.display()))?;
    Ok(())
}

/// A file counts as cached when it exists and is non-empty (a stale `.part` from
/// an interrupted run is ignored — `file_ready` checks the final name only).
fn file_ready(dest: &Path) -> bool {
    std::fs::metadata(dest).map(|m| m.len() > 0).unwrap_or(false)
}

/// Persist the resolved absolute path back into the *detected* selection so the
/// next launch hits the fast path. Best-effort; never fatal. Operator pins are
/// left untouched (their path is already authoritative).
fn persist_model_path(sel: Option<&Selection>, primary: &Path) {
    let Some(s) = sel else { return };
    if s.source != SelectionSource::Detected {
        return;
    }
    // Re-load the on-disk record to avoid clobbering a concurrent rewrite.
    if let Some(mut cur) = crate::hwdetect::load_selection() {
        let p = primary.to_string_lossy().to_string();
        if cur.model_path.as_deref() != Some(p.as_str()) {
            cur.model_path = Some(p);
            let _ = crate::hwdetect::save_selection(&cur);
        }
    }
}

fn is_stderr_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

fn print_progress(downloaded: u64, total: Option<u64>) {
    match total {
        Some(t) if t > 0 => {
            let pct = (downloaded as f64 / t as f64 * 100.0).min(100.0);
            eprint!(
                "\r\x1b[2m  {} / {} ({pct:.0}%)\x1b[0m\x1b[K",
                fmt_bytes(downloaded),
                fmt_bytes(t)
            );
        }
        _ => eprint!("\r\x1b[2m  {}\x1b[0m\x1b[K", fmt_bytes(downloaded)),
    }
    let _ = std::io::stderr().flush();
}

fn fmt_bytes(n: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let f = n as f64;
    if f >= GB {
        format!("{:.1} GB", f / GB)
    } else {
        format!("{:.0} MB", f / MB)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_is_under_aish() {
        let d = models_dir();
        assert!(d.ends_with("models"));
        assert!(d.parent().unwrap().ends_with(".aish"));
    }

    #[test]
    fn repo_slug_flattens_slashes() {
        assert_eq!(
            repo_slug("bartowski/Qwen2.5-14B-Instruct-GGUF"),
            "bartowski_Qwen2.5-14B-Instruct-GGUF"
        );
    }

    #[test]
    fn convention_filename_strips_gguf_suffix() {
        assert_eq!(
            convention_filename("bartowski/Qwen2.5-14B-Instruct-GGUF", "Q4_K_M"),
            "Qwen2.5-14B-Instruct-Q4_K_M.gguf"
        );
        // lowercase -gguf suffix + empty quant falls back to default.
        assert_eq!(
            convention_filename("acme/Foo-gguf", ""),
            "Foo-Q4_K_M.gguf"
        );
        // no GGUF suffix → stem is the whole last segment.
        assert_eq!(convention_filename("acme/Foo", "Q8_0"), "Foo-Q8_0.gguf");
    }

    #[test]
    fn quant_boundary_matching_is_strict() {
        assert!(quant_matches("qwen2.5-14b-instruct-q4_k_m.gguf", "q4_k_m"));
        // shard boundary
        assert!(quant_matches("model-q4_k_m-00001-of-00002.gguf", "q4_k_m"));
        // must NOT match a longer quant token
        assert!(!quant_matches("model-q4_k_m_l.gguf", "q4_k_m"));
        // a different quant entirely
        assert!(!quant_matches("model-q8_0.gguf", "q4_k_m"));
        // empty quant matches anything
        assert!(quant_matches("whatever.gguf", ""));
    }

    #[test]
    fn pick_single_file_for_quant() {
        let files = vec![
            "Qwen2.5-14B-Instruct-Q4_K_M.gguf".to_string(),
            "Qwen2.5-14B-Instruct-Q8_0.gguf".to_string(),
            "Qwen2.5-14B-Instruct-Q4_K_L.gguf".to_string(),
            "README.md".to_string(),
            "config.json".to_string(),
        ];
        assert_eq!(
            pick_gguf_files(&files, "Q4_K_M"),
            vec!["Qwen2.5-14B-Instruct-Q4_K_M.gguf".to_string()]
        );
    }

    #[test]
    fn pick_returns_all_shards_sorted() {
        let files = vec![
            "Llama-3.3-70B-Instruct-Q4_K_M-00002-of-00002.gguf".to_string(),
            "Llama-3.3-70B-Instruct-Q4_K_M-00001-of-00002.gguf".to_string(),
            "Llama-3.3-70B-Instruct-Q8_0-00001-of-00004.gguf".to_string(),
        ];
        assert_eq!(
            pick_gguf_files(&files, "Q4_K_M"),
            vec![
                "Llama-3.3-70B-Instruct-Q4_K_M-00001-of-00002.gguf".to_string(),
                "Llama-3.3-70B-Instruct-Q4_K_M-00002-of-00002.gguf".to_string(),
            ]
        );
    }

    #[test]
    fn pick_handles_subdir_shards_and_excludes_mmproj() {
        let files = vec![
            "Q4_K_M/Model-Q4_K_M-00001-of-00002.gguf".to_string(),
            "Q4_K_M/Model-Q4_K_M-00002-of-00002.gguf".to_string(),
            "mmproj-Model-Q4_K_M.gguf".to_string(),
        ];
        let picked = pick_gguf_files(&files, "Q4_K_M");
        assert_eq!(picked.len(), 2);
        assert!(picked.iter().all(|f| f.starts_with("Q4_K_M/")));
        assert!(picked.iter().all(|f| !basename_of(f).starts_with("mmproj")));
    }

    #[test]
    fn pick_empty_when_no_match() {
        let files = vec!["Model-Q8_0.gguf".to_string()];
        assert!(pick_gguf_files(&files, "Q4_K_M").is_empty());
    }

    #[test]
    fn parse_siblings_extracts_rfilenames() {
        let body = r#"{"id":"bartowski/x","siblings":[
            {"rfilename":"README.md"},
            {"rfilename":"Model-Q4_K_M.gguf"},
            {"rfilename":"Model-Q8_0.gguf"}
        ]}"#;
        assert_eq!(
            parse_siblings(body),
            vec![
                "README.md".to_string(),
                "Model-Q4_K_M.gguf".to_string(),
                "Model-Q8_0.gguf".to_string()
            ]
        );
        // malformed / missing siblings → empty, never panics.
        assert!(parse_siblings("not json").is_empty());
        assert!(parse_siblings(r#"{"id":"x"}"#).is_empty());
    }

    #[test]
    fn url_builders_are_well_formed() {
        assert_eq!(
            api_url("https://huggingface.co/", "owner/Repo"),
            "https://huggingface.co/api/models/owner/Repo"
        );
        assert_eq!(
            resolve_url("https://huggingface.co", "owner/Repo", "main", "sub/file.gguf"),
            "https://huggingface.co/owner/Repo/resolve/main/sub/file.gguf?download=true"
        );
    }

    #[test]
    fn check_url_allows_https_and_loopback_only() {
        assert!(check_url("https://huggingface.co/x").is_ok());
        assert!(check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(check_url("http://localhost/x").is_ok());
        assert!(check_url("http://huggingface.co/x").is_err());
        assert!(check_url("ftp://example.com/x").is_err());
    }

    #[test]
    fn fmt_bytes_switches_units() {
        assert_eq!(fmt_bytes(500 * 1024 * 1024), "500 MB");
        assert_eq!(fmt_bytes(9 * 1024 * 1024 * 1024), "9.0 GB");
    }
}
