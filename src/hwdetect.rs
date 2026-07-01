//! Hardware-aware local-model selection.
//!
//! When aish runs for the FIRST time, or when the operator switches to the
//! local backend on a machine that has never been profiled, we inspect the
//! hardware (RAM, logical CPUs, GPU/VRAM, OS/arch) and pick the best-fitting
//! GGUF model from a curated table. The methodology mirrors **whichllm**
//! (<https://github.com/Andyyyy64/whichllm>): compute a usable *memory budget*
//! (prefer GPU VRAM when present, otherwise a headroom-reserved slice of system
//! RAM), then choose the largest model whose quantized footprint fits that
//! budget.
//!
//! The choice is persisted to `~/.aish/config/local-model.json` and re-applied on later
//! launches, so detection runs once and the answer sticks. An operator-pinned
//! model — `AISH_LOCAL_MODEL_PATH` / `AISH_LOCAL_MODEL_ID`, or a `--model` flag
//! on a local launch — ALWAYS wins and is never overridden by auto-detection.
//!
//! This module is feature-independent (it compiles without the heavy `local`
//! feature) so detection, the colon command, and the `--detect-local-model`
//! flag all work in the default build; only actually *running* inference needs
//! `--features local`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Fallback model id when nothing has been detected yet — the smallest tier in
/// the ladder, so it runs on practically any machine. Only read on the
/// `local`-feature path, so the default build sees it as unused.
#[allow(dead_code)]
pub const DEFAULT_MODEL_ID: &str = "qwen2.5-0.5b-instruct";

// ---------------------------------------------------------------------------
// System profile
// ---------------------------------------------------------------------------

/// What kind of GPU (if any) we found. Drives whether the memory budget is sized
/// against dedicated VRAM (discrete) or a slice of system RAM (unified / none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Intel` is a reserved tier; not yet emitted by detection.
pub enum GpuKind {
    None,
    /// Apple Silicon — unified memory shared between CPU and GPU (Metal).
    Apple,
    Nvidia,
    Amd,
    Intel,
}

impl GpuKind {
    pub fn label(self) -> &'static str {
        match self {
            GpuKind::None => "cpu-only",
            GpuKind::Apple => "Apple Silicon (unified)",
            GpuKind::Nvidia => "NVIDIA",
            GpuKind::Amd => "AMD",
            GpuKind::Intel => "Intel",
        }
    }
}

/// A best-effort snapshot of the machine aish is running on.
#[derive(Debug, Clone)]
pub struct SystemProfile {
    pub os: String,
    pub arch: String,
    pub logical_cpus: usize,
    pub total_ram_mb: u64,
    pub gpu_kind: GpuKind,
    /// Dedicated VRAM in MB, when known (discrete GPUs). `None` for unified /
    /// CPU-only, where the budget is sized off system RAM instead.
    pub vram_mb: Option<u64>,
    /// GPU model string when probed (e.g. "NVIDIA GeForce RTX 4090"). Captured
    /// for future reporting; not yet surfaced in the default build.
    #[allow(dead_code)]
    pub gpu_name: Option<String>,
}

impl SystemProfile {
    /// One-line human summary, e.g.
    /// `linux/x86_64 · 16 cores · 32 GB RAM · NVIDIA (24 GB VRAM)`.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{}/{} · {} cores · {} RAM",
            self.os,
            self.arch,
            self.logical_cpus,
            fmt_gb(self.total_ram_mb),
        );
        match (self.gpu_kind, self.vram_mb) {
            (GpuKind::None, _) => s.push_str(" · cpu-only"),
            (kind, Some(v)) => s.push_str(&format!(" · {} ({} VRAM)", kind.label(), fmt_gb(v))),
            (kind, None) => s.push_str(&format!(" · {}", kind.label())),
        }
        s
    }
}

/// Detect the running machine's hardware. Never fails — every probe falls back
/// to a conservative default so a missing `/proc`, absent `nvidia-smi`, or an
/// unexpected OS still yields a usable profile.
pub fn detect() -> SystemProfile {
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let logical_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let total_ram_mb = detect_total_ram_mb();
    let (gpu_kind, vram_mb, gpu_name) = detect_gpu(&os, &arch);
    SystemProfile {
        os,
        arch,
        logical_cpus,
        total_ram_mb,
        gpu_kind,
        vram_mb,
        gpu_name,
    }
}

/// Total physical RAM in MB. Linux reads `/proc/meminfo`; macOS shells out to
/// `sysctl -n hw.memsize`; anything else falls back to a safe 8 GB so the
/// recommendation still lands on a small, broadly-runnable model.
fn detect_total_ram_mb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(kb) = parse_meminfo_kb(&contents) {
                return kb / 1024;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if let Ok(bytes) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                return bytes / (1024 * 1024);
            }
        }
    }
    8 * 1024
}

/// Parse `MemTotal:` (kB) out of `/proc/meminfo` contents.
fn parse_meminfo_kb(contents: &str) -> Option<u64> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok());
        }
    }
    None
}

/// Best-effort GPU probe. Apple Silicon is inferred from the macOS+aarch64
/// target; NVIDIA is probed via `nvidia-smi` when it is on `PATH`; AMD VRAM is
/// read from sysfs when present. Everything is optional and swallows errors.
fn detect_gpu(os: &str, arch: &str) -> (GpuKind, Option<u64>, Option<String>) {
    if os == "macos" && arch == "aarch64" {
        return (GpuKind::Apple, None, Some("Apple Silicon".to_string()));
    }
    // NVIDIA — only spawn nvidia-smi if it's actually on PATH (avoids a slow
    // ENOENT round-trip on machines without it).
    if on_path("nvidia-smi") {
        if let Ok(out) = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=memory.total,name",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some((vram, name)) = parse_nvidia_smi(&text) {
                return (GpuKind::Nvidia, Some(vram), Some(name));
            }
        }
    }
    // AMD — amdgpu exposes total VRAM in bytes via sysfs.
    #[cfg(target_os = "linux")]
    {
        for card in 0..4 {
            let p = format!("/sys/class/drm/card{card}/device/mem_info_vram_total");
            if let Ok(s) = std::fs::read_to_string(&p) {
                if let Ok(bytes) = s.trim().parse::<u64>() {
                    if bytes > 0 {
                        return (GpuKind::Amd, Some(bytes / (1024 * 1024)), None);
                    }
                }
            }
        }
    }
    (GpuKind::None, None, None)
}

/// Parse the first data row of `nvidia-smi --query-gpu=memory.total,name
/// --format=csv,noheader,nounits` → `(vram_mb, name)`.
fn parse_nvidia_smi(text: &str) -> Option<(u64, String)> {
    let line = text.lines().find(|l| !l.trim().is_empty())?;
    let mut it = line.splitn(2, ',');
    let vram = it.next()?.trim().parse::<u64>().ok()?;
    let name = it.next().map(|s| s.trim().to_string()).unwrap_or_default();
    Some((vram, name))
}

/// Is `bin` resolvable on `PATH`? Cheap directory scan; avoids spawning a
/// process just to discover it doesn't exist.
fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

// ---------------------------------------------------------------------------
// Recommendation
// ---------------------------------------------------------------------------

/// One curated model option keyed by the minimum memory budget (MB) at which it
/// is a sensible pick. The table is consulted largest-first.
struct ModelTier {
    /// Minimum usable memory budget (MB) to recommend this model. Includes
    /// headroom over the raw footprint for KV-cache + the OS.
    min_budget_mb: u64,
    model_id: &'static str,
    hf_repo: &'static str,
    quant: &'static str,
    approx_size_mb: u64,
    params: &'static str,
}

/// Curated GGUF ladder, descending. Footprints are ~Q4_K_M (the widely-used
/// quality/size sweet spot); `min_budget_mb` leaves headroom for the KV cache
/// and the OS on top of the raw weights.
const TIERS: &[ModelTier] = &[
    ModelTier {
        min_budget_mb: 44_000,
        model_id: "llama-3.3-70b-instruct",
        hf_repo: "bartowski/Llama-3.3-70B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 42_000,
        params: "70B",
    },
    ModelTier {
        min_budget_mb: 22_000,
        model_id: "qwen2.5-32b-instruct",
        hf_repo: "bartowski/Qwen2.5-32B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 19_800,
        params: "32B",
    },
    ModelTier {
        min_budget_mb: 18_500,
        model_id: "gemma-2-27b-it",
        hf_repo: "bartowski/gemma-2-27b-it-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 16_600,
        params: "27B",
    },
    ModelTier {
        min_budget_mb: 11_000,
        model_id: "qwen2.5-14b-instruct",
        hf_repo: "bartowski/Qwen2.5-14B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 9_000,
        params: "14B",
    },
    ModelTier {
        min_budget_mb: 6_500,
        model_id: "llama-3.1-8b-instruct",
        hf_repo: "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 4_900,
        params: "8B",
    },
    ModelTier {
        min_budget_mb: 3_500,
        model_id: "llama-3.2-3b-instruct",
        hf_repo: "bartowski/Llama-3.2-3B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 2_200,
        params: "3B",
    },
    ModelTier {
        min_budget_mb: 2_000,
        model_id: "qwen2.5-1.5b-instruct",
        hf_repo: "bartowski/Qwen2.5-1.5B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 1_100,
        params: "1.5B",
    },
    ModelTier {
        min_budget_mb: 0,
        model_id: "qwen2.5-0.5b-instruct",
        hf_repo: "bartowski/Qwen2.5-0.5B-Instruct-GGUF",
        quant: "Q4_K_M",
        approx_size_mb: 400,
        params: "0.5B",
    },
];

/// The usable memory budget (MB) for sizing a local model, whichllm-style:
/// - **discrete GPU** (NVIDIA/AMD with known VRAM): 90% of VRAM (full offload)
///   OR a headroom-reserved slice of system RAM for CPU/partial offload —
///   whichever is larger, so a big-RAM box isn't capped by a small GPU.
/// - **Apple unified memory**: 70% of system RAM (Metal can map most of it).
/// - **CPU-only**: system RAM minus a 2 GB OS reserve, times 70%.
pub fn memory_budget_mb(p: &SystemProfile) -> u64 {
    let cpu_budget = p.total_ram_mb.saturating_sub(2048) * 7 / 10;
    match p.gpu_kind {
        GpuKind::Apple => (p.total_ram_mb * 7 / 10).max(cpu_budget),
        GpuKind::Nvidia | GpuKind::Amd => {
            let gpu_budget = p.vram_mb.map(|v| v * 9 / 10).unwrap_or(0);
            gpu_budget.max(cpu_budget)
        }
        // Intel iGPU / unknown: treat as CPU-only.
        GpuKind::Intel | GpuKind::None => cpu_budget,
    }
}

/// Select the largest tier whose `min_budget_mb` fits the budget.
fn pick_tier(budget_mb: u64) -> &'static ModelTier {
    TIERS
        .iter()
        .find(|t| budget_mb >= t.min_budget_mb)
        // The last tier has `min_budget_mb == 0`, so this is always `Some`; the
        // fallback keeps the function total without an unwrap.
        .unwrap_or(&TIERS[TIERS.len() - 1])
}

/// A concrete model recommendation for a profile.
#[derive(Debug, Clone)]
pub struct Recommendation {
    pub model_id: String,
    pub hf_repo: String,
    pub quant: String,
    pub approx_size_mb: u64,
    pub params: String,
    pub budget_mb: u64,
    /// Human-readable "why this model" line. Folded into the persisted
    /// `Selection`/report rather than read directly off the recommendation.
    #[allow(dead_code)]
    pub rationale: String,
}

/// Recommend the best-fitting local model for a system profile.
pub fn recommend(p: &SystemProfile) -> Recommendation {
    let budget_mb = memory_budget_mb(p);
    let tier = pick_tier(budget_mb);
    let basis = match (p.gpu_kind, p.vram_mb) {
        (GpuKind::Apple, _) => format!("{} unified memory", fmt_gb(p.total_ram_mb)),
        (GpuKind::Nvidia | GpuKind::Amd, Some(v)) => {
            format!("{} {} VRAM", p.gpu_kind.label(), fmt_gb(v))
        }
        _ => format!("{} system RAM (CPU)", fmt_gb(p.total_ram_mb)),
    };
    let rationale = format!(
        "{} budget from {} → {} {} fits (~{})",
        fmt_gb(budget_mb),
        basis,
        tier.params,
        tier.quant,
        fmt_gb(tier.approx_size_mb),
    );
    Recommendation {
        model_id: tier.model_id.to_string(),
        hf_repo: tier.hf_repo.to_string(),
        quant: tier.quant.to_string(),
        approx_size_mb: tier.approx_size_mb,
        params: tier.params.to_string(),
        budget_mb,
        rationale,
    }
}

// ---------------------------------------------------------------------------
// Persisted selection
// ---------------------------------------------------------------------------

/// How the active selection was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSource {
    /// Chosen by hardware auto-detection.
    Detected,
    /// Pinned by the operator (env var or `--model`); never auto-overridden.
    Operator,
}

/// The persisted local-model choice (`~/.aish/config/local-model.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    pub model_id: String,
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub quant: String,
    pub source: SelectionSource,
    #[serde(default)]
    pub detected_at: u64,
    #[serde(default)]
    pub budget_mb: u64,
    #[serde(default)]
    pub profile_summary: String,
    #[serde(default)]
    pub params: String,
    #[serde(default)]
    pub hf_repo: String,
    #[serde(default)]
    pub approx_size_mb: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `~/.aish` config home (mirrors `main::aish_dir`).
pub fn aish_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aish")
}

/// The config subdirectory, `~/.aish/config/`, where the persisted local-model
/// selection now lives (mirrors `db_paths::db_dir`'s pattern). Best-effort
/// creates the directory on every call via `fs::create_dir_all` (idempotent),
/// so callers can write straight into the returned path without a separate
/// mkdir. A creation failure is swallowed here — the subsequent write surfaces
/// a precise, actionable error instead.
pub fn config_dir() -> PathBuf {
    let dir = aish_dir().join("config");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path of the persisted selection file: `~/.aish/config/local-model.json`.
pub fn selection_path() -> PathBuf {
    config_dir().join("local-model.json")
}

/// Legacy path of the persisted selection: `~/.aish/local-model.json`. Read as
/// a fallback so a selection written before the move to `~/.aish/config/`
/// survives the upgrade.
fn legacy_selection_path() -> PathBuf {
    aish_dir().join("local-model.json")
}

/// Load the persisted selection, if any. Prefers the current
/// `~/.aish/config/local-model.json`, falling back to the legacy
/// `~/.aish/local-model.json`. Returns `None` on a missing or unparseable file
/// (treated as "never selected").
pub fn load_selection() -> Option<Selection> {
    let raw = std::fs::read_to_string(selection_path())
        .or_else(|_| std::fs::read_to_string(legacy_selection_path()))
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist a selection to `~/.aish/config/local-model.json` (pretty-printed).
pub fn save_selection(sel: &Selection) -> Result<()> {
    // `selection_path()` -> `config_dir()` creates `~/.aish/config/` for us.
    let path = selection_path();
    let json = serde_json::to_string_pretty(sel).context("serializing local-model selection")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The currently-selected local model id — the persisted choice, or the
/// historical default when nothing has been selected yet. Used by the local
/// backend's `describe()` / `model()` so the UI reflects the real selection.
#[allow(dead_code)] // consumed only on the `local`-feature backend path.
pub fn selected_model_id() -> String {
    load_selection()
        .map(|s| s.model_id)
        .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string())
}

/// Look up the curated Hugging Face repo + quant for a known `model_id`, so a
/// recorded selection (which may predate the `hf_repo`/`quant` fields, or carry
/// only a bare id) can still be resolved to downloadable weights. Returns
/// `(hf_repo, quant)` when the id matches a tier, else `None`. Consumed by
/// `crate::modelfetch` on the `local`-feature path.
#[allow(dead_code)]
pub fn repo_and_quant_for(model_id: &str) -> Option<(String, String)> {
    TIERS
        .iter()
        .find(|t| t.model_id == model_id)
        .map(|t| (t.hf_repo.to_string(), t.quant.to_string()))
}


/// An operator pin, if one is in force. `--model` (passed through as `cli_model`
/// on a local launch) wins over env, then `AISH_LOCAL_MODEL_PATH` (an explicit
/// GGUF file), then `AISH_LOCAL_MODEL_ID`. Returns `(model_id, model_path?)`.
fn operator_pin(cli_model: Option<&str>) -> Option<(String, Option<String>)> {
    if let Some(m) = cli_model.filter(|s| !s.trim().is_empty()) {
        return Some((m.to_string(), None));
    }
    if let Ok(path) = std::env::var("AISH_LOCAL_MODEL_PATH") {
        if !path.trim().is_empty() {
            let id = PathBuf::from(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            return Some((id, Some(path)));
        }
    }
    if let Ok(id) = std::env::var("AISH_LOCAL_MODEL_ID") {
        if !id.trim().is_empty() {
            return Some((id, None));
        }
    }
    None
}

/// Ensure a local model is selected and return the active selection.
///
/// Precedence (matching the operator's requirement that a pinned model is never
/// overridden):
/// 1. An operator pin (`--model` on a local launch, or `AISH_LOCAL_MODEL_*`) —
///    recorded as [`SelectionSource::Operator`].
/// 2. Otherwise, unless `force`, a previously persisted selection.
/// 3. Otherwise, detect hardware, recommend a model, persist, and return it.
///
/// `force = true` re-runs detection even when a stored selection exists, but
/// still yields to an operator pin (that's the whole point of a pin).
pub fn ensure_selected(force: bool, cli_model: Option<&str>) -> Result<Selection> {
    if let Some((model_id, model_path)) = operator_pin(cli_model) {
        let sel = Selection {
            model_id,
            model_path,
            quant: String::new(),
            source: SelectionSource::Operator,
            detected_at: now_secs(),
            budget_mb: 0,
            profile_summary: "operator-pinned".to_string(),
            params: String::new(),
            hf_repo: String::new(),
            approx_size_mb: 0,
        };
        // Best-effort persist; a write failure must not block the pin taking
        // effect for this run.
        let _ = save_selection(&sel);
        return Ok(sel);
    }

    if !force {
        if let Some(existing) = load_selection() {
            return Ok(existing);
        }
    }

    let profile = detect();
    let rec = recommend(&profile);
    let sel = Selection {
        model_id: rec.model_id,
        model_path: None,
        quant: rec.quant,
        source: SelectionSource::Detected,
        detected_at: now_secs(),
        budget_mb: rec.budget_mb,
        profile_summary: profile.summary(),
        params: rec.params,
        hf_repo: rec.hf_repo,
        approx_size_mb: rec.approx_size_mb,
    };
    save_selection(&sel)?;
    Ok(sel)
}

/// Export the selection as `AISH_LOCAL_*` env hints so a local backend built
/// afterwards in this process picks it up.
pub fn apply_env(sel: &Selection) {
    // SAFETY: invoked during single-threaded startup or interactive command
    // handling, before any local backend reads these vars. Best-effort; only
    // touches aish's own AISH_LOCAL_* hint variables.
    unsafe {
        std::env::set_var("AISH_LOCAL_MODEL_ID", &sel.model_id);
        if let Some(path) = &sel.model_path {
            std::env::set_var("AISH_LOCAL_MODEL_PATH", path);
        }
    }
}

/// One-line confirmation, e.g.
/// `↳ local model: llama-3.1-8b-instruct (Q4_K_M, ~4.9 GB)`.
#[allow(dead_code)] // used on the `local`-feature path and in tests.
pub fn short_line(sel: &Selection) -> String {
    let size = if sel.approx_size_mb > 0 {
        format!(", ~{}", fmt_gb(sel.approx_size_mb))
    } else {
        String::new()
    };
    let quant = if sel.quant.is_empty() {
        String::new()
    } else {
        format!(" ({}{})", sel.quant, size)
    };
    match sel.source {
        SelectionSource::Operator => format!("↳ local model: {} (operator-pinned)", sel.model_id),
        SelectionSource::Detected => format!("↳ local model: {}{}", sel.model_id, quant),
    }
}

/// Multi-line human report for `:model-detect` / `--detect-local-model`.
pub fn report(sel: &Selection) -> String {
    let mut out = String::new();
    match sel.source {
        SelectionSource::Operator => {
            out.push_str("local model is operator-pinned (auto-detection skipped)\n");
            out.push_str(&format!("  model:   {}\n", sel.model_id));
            if let Some(p) = &sel.model_path {
                out.push_str(&format!("  path:    {p}\n"));
            }
            out.push_str("  source:  operator (AISH_LOCAL_MODEL_* / --model)");
        }
        SelectionSource::Detected => {
            out.push_str("detected hardware and selected a local model:\n");
            out.push_str(&format!("  system:  {}\n", sel.profile_summary));
            out.push_str(&format!("  budget:  {}\n", fmt_gb(sel.budget_mb)));
            out.push_str(&format!(
                "  model:   {} ({}, {}, ~{})\n",
                sel.model_id,
                sel.params,
                sel.quant,
                fmt_gb(sel.approx_size_mb)
            ));
            if !sel.hf_repo.is_empty() {
                out.push_str(&format!("  gguf:    {}\n", sel.hf_repo));
            }
            out.push_str("  source:  hardware auto-detection");
        }
    }
    out
}

/// Format MB as a compact GB string (`512 MB` under 1 GB, else `N.N GB`).
fn fmt_gb(mb: u64) -> String {
    if mb < 1024 {
        format!("{mb} MB")
    } else {
        let gb = mb as f64 / 1024.0;
        if (gb.round() - gb).abs() < 0.05 {
            format!("{} GB", gb.round() as u64)
        } else {
            format!("{gb:.1} GB")
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(ram_mb: u64, gpu: GpuKind, vram: Option<u64>) -> SystemProfile {
        SystemProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            logical_cpus: 8,
            total_ram_mb: ram_mb,
            gpu_kind: gpu,
            vram_mb: vram,
            gpu_name: None,
        }
    }

    #[test]
    fn parses_meminfo_memtotal() {
        let sample = "MemTotal:       32791234 kB\nMemFree:  100 kB\n";
        assert_eq!(parse_meminfo_kb(sample), Some(32_791_234));
        assert_eq!(parse_meminfo_kb("MemFree: 10 kB"), None);
        assert_eq!(parse_meminfo_kb(""), None);
    }

    #[test]
    fn parses_nvidia_smi_row() {
        let out = "24564, NVIDIA GeForce RTX 4090\n";
        assert_eq!(
            parse_nvidia_smi(out),
            Some((24564, "NVIDIA GeForce RTX 4090".to_string()))
        );
        // blank lines are skipped; missing name → empty string
        assert_eq!(parse_nvidia_smi("\n8192\n"), Some((8192, String::new())));
        assert_eq!(parse_nvidia_smi("garbage"), None);
    }

    #[test]
    fn cpu_only_budget_reserves_os_headroom() {
        // 16 GB CPU-only: (16384 - 2048) * 0.7 = 10035 MB.
        let p = profile(16 * 1024, GpuKind::None, None);
        assert_eq!(memory_budget_mb(&p), (16 * 1024 - 2048) * 7 / 10);
    }

    #[test]
    fn discrete_gpu_uses_max_of_vram_and_ram() {
        // Small GPU, big RAM → RAM budget wins (CPU/partial offload).
        let p = profile(64 * 1024, GpuKind::Nvidia, Some(8 * 1024));
        let cpu = (64 * 1024 - 2048) * 7 / 10;
        assert_eq!(memory_budget_mb(&p), cpu);
        // Big GPU, modest RAM → VRAM budget wins.
        let p = profile(16 * 1024, GpuKind::Nvidia, Some(24 * 1024));
        assert_eq!(memory_budget_mb(&p), 24 * 1024 * 9 / 10);
    }

    #[test]
    fn apple_unified_uses_ram_share() {
        let p = profile(32 * 1024, GpuKind::Apple, None);
        assert_eq!(memory_budget_mb(&p), 32 * 1024 * 7 / 10);
    }

    #[test]
    fn tiers_are_descending_and_total() {
        // Monotonically descending thresholds, and the last tier is the floor.
        for w in TIERS.windows(2) {
            assert!(w[0].min_budget_mb > w[1].min_budget_mb);
        }
        assert_eq!(TIERS[TIERS.len() - 1].min_budget_mb, 0);
        // A zero budget still resolves to the smallest model.
        assert_eq!(pick_tier(0).params, "0.5B");
    }

    #[test]
    fn recommends_by_memory_tier() {
        // Tiny box → smallest model.
        let r = recommend(&profile(2 * 1024, GpuKind::None, None));
        assert_eq!(r.params, "0.5B");
        // 16 GB CPU box (~10 GB budget) → 8B.
        let r = recommend(&profile(16 * 1024, GpuKind::None, None));
        assert_eq!(r.params, "8B");
        assert_eq!(r.model_id, "llama-3.1-8b-instruct");
        // 64 GB CPU box (~44 GB budget) → 70B.
        let r = recommend(&profile(64 * 1024, GpuKind::None, None));
        assert_eq!(r.params, "70B");
        // 24 GB NVIDIA → ~22.1 GB budget clears the 32B tier (22 GB floor); a
        // 32B Q4_K_M (~19.8 GB) fits inside 24 GB of VRAM.
        let r = recommend(&profile(32 * 1024, GpuKind::Nvidia, Some(24 * 1024)));
        assert_eq!(r.params, "32B");
        // Rationale mentions the chosen size.
        assert!(r.rationale.contains("32B"));
    }

    #[test]
    fn operator_pin_precedence_is_pure() {
        // --model wins regardless of env (we don't mutate env here; cli arg only).
        let pin = operator_pin(Some("my-custom-model"));
        assert_eq!(pin, Some(("my-custom-model".to_string(), None)));
        // Empty / whitespace cli model is ignored.
        assert_eq!(operator_pin(Some("   ")), None);
    }

    #[test]
    fn selection_path_lives_under_config_dir() {
        // The persisted selection now lives at ~/.aish/config/local-model.json,
        // not loose in the config home (~/.aish/local-model.json).
        let path = selection_path();
        assert!(path.ends_with("config/local-model.json"), "{}", path.display());
        assert!(config_dir().ends_with("config"));
        // config_dir() is a strict child of aish_dir().
        assert_eq!(config_dir().parent(), Some(aish_dir().as_path()));
    }

    #[test]
    fn selection_round_trips_through_json() {
        let sel = Selection {
            model_id: "llama-3.1-8b-instruct".into(),
            model_path: None,
            quant: "Q4_K_M".into(),
            source: SelectionSource::Detected,
            detected_at: 123,
            budget_mb: 10035,
            profile_summary: "linux/x86_64 · 8 cores · 16 GB RAM · cpu-only".into(),
            params: "8B".into(),
            hf_repo: "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF".into(),
            approx_size_mb: 4900,
        };
        let json = serde_json::to_string(&sel).unwrap();
        let back: Selection = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model_id, sel.model_id);
        assert_eq!(back.source, SelectionSource::Detected);
        assert_eq!(back.budget_mb, 10035);
        // snake_case enum on the wire.
        assert!(json.contains("\"detected\""));
    }

    #[test]
    fn detected_selection_partial_json_uses_defaults() {
        // A forward/old file with only the required fields still loads.
        let json = r#"{"model_id":"x","source":"operator"}"#;
        let sel: Selection = serde_json::from_str(json).unwrap();
        assert_eq!(sel.model_id, "x");
        assert_eq!(sel.source, SelectionSource::Operator);
        assert_eq!(sel.budget_mb, 0);
        assert!(sel.model_path.is_none());
    }

    #[test]
    fn fmt_gb_is_compact() {
        assert_eq!(fmt_gb(512), "512 MB");
        assert_eq!(fmt_gb(16 * 1024), "16 GB");
        assert_eq!(fmt_gb(10035), "9.8 GB");
    }

    #[test]
    fn short_line_distinguishes_source() {
        let detected = Selection {
            model_id: "llama-3.1-8b-instruct".into(),
            model_path: None,
            quant: "Q4_K_M".into(),
            source: SelectionSource::Detected,
            detected_at: 0,
            budget_mb: 0,
            profile_summary: String::new(),
            params: "8B".into(),
            hf_repo: String::new(),
            approx_size_mb: 4900,
        };
        assert!(short_line(&detected).contains("Q4_K_M"));
        let pinned = Selection {
            source: SelectionSource::Operator,
            ..detected
        };
        assert!(short_line(&pinned).contains("operator-pinned"));
    }
}
