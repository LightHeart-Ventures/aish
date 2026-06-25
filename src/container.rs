//! Container worker runtime — hermetic, rootless execution for background
//! coordinators (S9.1).
//!
//! Where [`crate::worker`] re-execs `aish` as a plain HOST subprocess in
//! `--coordinator` mode (same cwd, inherited env, `setrlimit` caps), this module
//! provides an additive, pluggable CONTAINER backend: each worker runs in its
//! own rootless container with a stable name + labels, a mounted state volume,
//! and resource caps mapped onto cgroup flags. The container path is purely
//! additive — when no runtime is selected the worker takes the existing host
//! path BYTE-FOR-BYTE (AC1).
//!
//! Design (per the engineering spec):
//! - **Shell-out, no daemon SDK.** Both impls invoke the `podman`/`docker` CLI
//!   directly (mirroring how [`crate::update`] shells out to `gh`), preserving
//!   the single-binary, dependency-light ethos.
//! - **Reuse every existing seam.** The container's command is the SAME
//!   coordinator argv the host path builds (`coordinator_argv` in `worker.rs`),
//!   so only the execution vehicle changes; the stdout-as-result capture and the
//!   `🔧`/`🗨`/`📦` stderr pulse machinery are untouched.
//! - **Identity + volume.** Deterministic name `aish-<session>-<worker>` and a
//!   schema-versioned label set make a worker uniquely resolvable
//!   (`ps --filter label=aish.worker_id=<id>`); a per-worker host dir mounted at
//!   `/aish/state` is the persistent volume S9.3 writes its transcript into.
//!
//! Scope note: S9.1 runs the container ATTACHED (`run`, not `-d`) so the
//! coordinator's stdout is recovered directly by the existing capped capture —
//! this satisfies AC2 without depending on S9.3's `/aish/state/result.txt`
//! writer. The DETACHED, shell-survival lifecycle (write the answer to the
//! volume, read it back after `wait`) is S9.4's concern and layers on top of
//! this abstraction without changing it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

/// A container engine aish knows how to drive. The "no runtime / host
/// subprocess" case is represented by the ABSENCE of a `Runtime`
/// (`Option<Runtime>` / [`Selection::Host`]), never a variant here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runtime {
    Podman,
    Docker,
}

impl Runtime {
    /// The CLI binary name this runtime shells out to.
    pub fn bin(self) -> &'static str {
        match self {
            Runtime::Podman => "podman",
            Runtime::Docker => "docker",
        }
    }

    /// Parse the `AISH_CONTAINER_RUNTIME` selector value. `podman`/`docker` pick
    /// that engine explicitly; `none` (or `host`) forces the host-subprocess
    /// path; anything else (incl. unset) is `None` → auto-detect. Pure.
    pub fn parse_selector(raw: Option<&str>) -> SelectorPref {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("podman") => SelectorPref::Force(Runtime::Podman),
            Some("docker") => SelectorPref::Force(Runtime::Docker),
            Some("none") | Some("host") => SelectorPref::Host,
            _ => SelectorPref::Auto,
        }
    }
}

/// The parsed intent of the `AISH_CONTAINER_RUNTIME` knob (pre-PATH-probe).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectorPref {
    /// Use this engine, no auto-detection.
    Force(Runtime),
    /// `none`/`host` — force the host-subprocess path (AC1 byte-for-byte).
    Host,
    /// Unset / unrecognized — auto-detect (prefer podman, else docker, else host).
    Auto,
}

/// The resolved execution vehicle for a worker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    /// Today's host subprocess path (no container).
    Host,
    /// Run in a container under the given engine.
    Container(Runtime),
}

/// Is `bin` present and runnable on PATH? Cheap `--version` probe; false on any
/// error (missing, not executable, …). Mirrors `update::gh_available`.
pub fn runtime_on_path(rt: Runtime) -> bool {
    std::process::Command::new(rt.bin())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Auto-detect the preferred runtime on PATH per AC1: prefer **podman** (rootless,
/// daemonless), else **docker**, else `None` (no engine → host path). Honors an
/// explicit `AISH_CONTAINER_RUNTIME=none` by short-circuiting to `None`.
#[allow(dead_code)] // AC1 auto-detect entry point; the engaged cutover uses resolve_selection.
pub fn detect_runtime() -> Option<Runtime> {
    match Runtime::parse_selector(std::env::var("AISH_CONTAINER_RUNTIME").ok().as_deref()) {
        SelectorPref::Force(rt) => runtime_on_path(rt).then_some(rt),
        SelectorPref::Host => None,
        SelectorPref::Auto => {
            if runtime_on_path(Runtime::Podman) {
                Some(Runtime::Podman)
            } else if runtime_on_path(Runtime::Docker) {
                Some(Runtime::Docker)
            } else {
                None
            }
        }
    }
}

/// Decide the execution vehicle for a worker given the selector value and which
/// engines are on PATH. Pure (the PATH facts are injected) so the precedence
/// matrix is unit-testable without a daemon. AC1:
/// - `none`/`host` → always [`Selection::Host`] (byte-for-byte).
/// - `podman`/`docker` → that engine when present, else fall back to host.
/// - unset/auto → podman-if-present, else docker-if-present, else host.
///
/// The `engaged` gate is the S9.1 safety valve: the live execution CUTOVER is
/// opt-in (an explicit `podman`/`docker` selector) until the dependent cards
/// (S9.3 state volume writer, S9.4 detached survival) land. With `engaged=false`
/// (the default, `Auto`), auto-detection still REPORTS the runtime via
/// [`detect_runtime`] but a worker keeps the host path — so installing Docker
/// never silently changes background-worker behavior mid-sprint.
pub fn resolve_selection(
    pref: SelectorPref,
    podman_present: bool,
    docker_present: bool,
) -> Selection {
    match pref {
        SelectorPref::Host => Selection::Host,
        SelectorPref::Force(Runtime::Podman) => {
            if podman_present { Selection::Container(Runtime::Podman) } else { Selection::Host }
        }
        SelectorPref::Force(Runtime::Docker) => {
            if docker_present { Selection::Container(Runtime::Docker) } else { Selection::Host }
        }
        // Auto: report-only (host execution) until the cutover is enabled — see
        // the `engaged` note above. The auto-preference order itself lives in
        // `detect_runtime`; here Auto deliberately resolves to Host so a present
        // engine doesn't hijack the default path before S9.3/S9.4.
        SelectorPref::Auto => Selection::Host,
    }
}

/// The live execution vehicle a worker WOULD use right now, resolved from the
/// same inputs `run_worker` feeds `resolve_selection`: the `AISH_CONTAINER_RUNTIME`
/// selector plus which engines are on PATH. `:update --drain` reads this to know
/// whether background workers are containerized (and so survive a shell restart)
/// or host subprocesses (which die on restart, gating the AC8 confirmation).
pub fn current_selection() -> Selection {
    resolve_selection(
        Runtime::parse_selector(std::env::var("AISH_CONTAINER_RUNTIME").ok().as_deref()),
        runtime_on_path(Runtime::Podman),
        runtime_on_path(Runtime::Docker),
    )
}

/// True when background workers run in a container (and thus keep running across
/// a `:update --drain` shell restart); false for the host-subprocess path.
pub fn current_backend_is_container() -> bool {
    matches!(current_selection(), Selection::Container(_))
}

/// The image tag for a worker, pinned to the running aish version so a new build
/// rebuilds the image (`current_version()` from `update.rs`). Pure.
pub fn image_tag(version: &str) -> String {
    format!("aish-worker:{version}")
}

/// The deterministic container NAME for a worker: `aish-<session>-<worker>`
/// (AC3). The session + worker ids are already filesystem-/DNS-safe opaque
/// tokens; we still sanitize defensively so a stray char can't produce an
/// invalid container name. Pure → unit-tested.
pub fn container_name(session_id: &str, worker_id: &str) -> String {
    format!("aish-{}-{}", sanitize_token(session_id), sanitize_token(worker_id))
}

/// Sanitize an id into the `[A-Za-z0-9_.-]` set container engines accept in a
/// `--name`, collapsing anything else to `-`. Pure.
fn sanitize_token(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect()
}

/// The schema-versioned label set stamped on every worker container (AC3). These
/// make a worker uniquely resolvable by `--filter label=aish.worker_id=<id>` and
/// give S9.5 discovery a stable, greppable identity. No secrets ever go in labels
/// — ids, the repo key, and a timestamp only. Pure given its inputs.
pub fn worker_labels(
    worker_id: &str,
    session_id: &str,
    repo_key: &str,
    task_card_id: Option<&str>,
    created_at: &str,
) -> Vec<(String, String)> {
    let mut labels = vec![
        ("aish.schema".to_string(), "1".to_string()),
        ("aish.worker_id".to_string(), worker_id.to_string()),
        ("aish.session_id".to_string(), session_id.to_string()),
        ("aish.repo_key".to_string(), repo_key.to_string()),
        ("aish.created_at".to_string(), created_at.to_string()),
    ];
    if let Some(card) = task_card_id.filter(|c| !c.is_empty()) {
        labels.push(("aish.task_card_id".to_string(), card.to_string()));
    }
    labels
}

/// The default container network for `(runtime, os)` when `AISH_WORKER_NETWORK`
/// is unset (AC6). On Linux the host net namespace is the cheapest egress with
/// parity to the host path; on macOS the `podman machine` VM can't share the
/// host net, so podman uses its rootless `slirp4netns` and docker its `bridge`.
/// `os` is `std::env::consts::OS`. Pure → unit-tested.
pub fn default_network(runtime: Runtime, os: &str) -> &'static str {
    match (runtime, os) {
        (_, "linux") => "host",
        (Runtime::Podman, _) => "slirp4netns",
        (Runtime::Docker, _) => "bridge",
    }
}

/// Resource-cap flags for the worker container (AC6), mapped from the same env
/// knobs the host path reads. A `0`/`None` value omits the flag ("no limit").
/// Returned as a flat argv fragment so it splices into the `run` argv. Pure.
pub fn resource_flags(mem_mb: u64, cpus: Option<f64>, pids_limit: Option<u64>) -> Vec<String> {
    let mut out = Vec::new();
    if mem_mb > 0 {
        out.push("--memory".to_string());
        out.push(format!("{mem_mb}m"));
    }
    if let Some(c) = cpus.filter(|c| *c > 0.0) {
        out.push("--cpus".to_string());
        // Trim a trailing `.0` so `2.0` reads as `2` (cosmetic; both are valid).
        out.push(format!("{c}"));
    }
    if let Some(p) = pids_limit.filter(|p| *p > 0) {
        out.push("--pids-limit".to_string());
        out.push(p.to_string());
    }
    out
}

/// Everything needed to launch one worker container. Owns its data so the
/// spawning task is self-contained (mirrors `WorkerSpec`).
#[derive(Clone, Debug)]
pub struct ContainerSpec {
    /// Deterministic `aish-<session>-<worker>` name (AC3).
    pub name: String,
    /// `aish-worker:<version>` image tag (AC5).
    pub image: String,
    /// The coordinator argv (the SAME vector the host path execs) — becomes the
    /// container's command.
    pub argv: Vec<String>,
    /// Schema-versioned labels (AC3).
    pub labels: Vec<(String, String)>,
    /// Host dir mounted at `/aish/state` — the persistent state volume (AC4).
    pub state_volume_host: PathBuf,
    /// Path INSIDE the container the volume mounts at. Fixed at `/aish/state`.
    pub state_mount: String,
    /// Host dir bind-mounted at `workdir` so the in-container coordinator sees
    /// the project tree (the isolated worktree, or the session cwd). `None` runs
    /// with only the state volume. S9.1 mounts the run cwd read-write here; the
    /// volume-seeded clone is a later refinement.
    pub work_volume_host: Option<PathBuf>,
    /// 0600 env-file holding secret env (never argv, kept out of `ps`).
    pub env_file: Option<PathBuf>,
    /// Non-secret inline env (`-e KEY=VAL`).
    pub env_inline: Vec<(String, String)>,
    /// Memory cap in MB (AC6); 0 = no limit.
    pub mem_mb: u64,
    /// CPU cap (AC6); None = host default.
    pub cpus: Option<f64>,
    /// PID cap (AC6); None = no limit.
    pub pids_limit: Option<u64>,
    /// `--network` value (AC6).
    pub network: String,
    /// Working dir inside the container.
    pub workdir: String,
}

/// The in-container path the state volume always mounts at (AC4).
pub const STATE_MOUNT: &str = "/aish/state";

/// Build the full `run` argv (everything AFTER the `podman`/`docker` binary) for
/// an ATTACHED worker launch (S9.1). Pure — no IO — so the exact flag set is
/// unit-testable. Security posture (AC6/§8): rootless, `--cap-drop=ALL`, never
/// `--privileged`, secrets only via `--env-file`, the state dir the only mount.
/// `--rm` is set: S9.1 runs attached and finalizes here; the detached,
/// keep-on-changes lifecycle is S9.4.
pub fn run_argv(spec: &ContainerSpec) -> Vec<String> {
    let mut a: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), spec.name.clone()];
    // Identity labels.
    for (k, v) in &spec.labels {
        a.push("--label".into());
        a.push(format!("{k}={v}"));
    }
    // Resource caps.
    a.extend(resource_flags(spec.mem_mb, spec.cpus, spec.pids_limit));
    // Network.
    a.push("--network".into());
    a.push(spec.network.clone());
    // Hardening.
    a.push("--cap-drop=ALL".into());
    // Secrets via env-file (0600), non-secrets inline.
    if let Some(ef) = &spec.env_file {
        a.push("--env-file".into());
        a.push(ef.to_string_lossy().into_owned());
    }
    for (k, v) in &spec.env_inline {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    // Persistent state volume (AC4).
    a.push("-v".into());
    a.push(format!("{}:{}", spec.state_volume_host.display(), spec.state_mount));
    // Project tree (the worktree / cwd) bind-mounted at the workdir, so the
    // coordinator runs against the same files the host path would.
    if let Some(work) = &spec.work_volume_host {
        a.push("-v".into());
        a.push(format!("{}:{}", work.display(), spec.workdir));
    }
    // Workdir.
    a.push("-w".into());
    a.push(spec.workdir.clone());
    // Image, then the coordinator argv as the container command.
    a.push(spec.image.clone());
    a.extend(spec.argv.iter().cloned());
    a
}

/// Map a finished container's exit CODE to a human failure note, naming the
/// common container-specific exits the host path's `describe_failure` doesn't
/// know about: 137 (SIGKILL — almost always the cgroup OOM-killer hitting
/// `--memory`) and 125 (the engine itself failed to run the container). Returns
/// `None` for a clean exit (0) so the caller keeps the captured stdout as the
/// result. Pure → unit-tested.
#[allow(dead_code)] // S9.4 detached-lifecycle exit mapping (137=OOM, 125=engine).
pub fn describe_exit(code: i32) -> Option<String> {
    match code {
        0 => None,
        137 => Some(
            "worker container was OOM-killed (exit 137) — it hit the --memory cap; \
             raise AISH_WORKER_MEM_MB or split the task."
                .to_string(),
        ),
        125 => Some(
            "container engine failed to run the worker (exit 125) — check the image \
             and runtime (docker/podman) availability."
                .to_string(),
        ),
        n => Some(format!("worker container exited unsuccessfully (exit {n}).")),
    }
}

/// Whether the image `tag` already exists locally (shell-out probe). Used for
/// build-on-first-use: a missing image triggers `make worker-image`. False on
/// any error.
pub fn image_exists(rt: Runtime, tag: &str) -> bool {
    std::process::Command::new(rt.bin())
        .args(["image", "inspect", tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A discovered worker container — the unit S9.5 discovery scans by label, and
/// what `list` returns.
#[derive(Clone, Debug)]
#[allow(dead_code)] // S9.5 discovery scans containers by label into these.
pub struct ContainerHandle {
    pub id: String,
    pub name: String,
    pub labels: HashMap<String, String>,
}

/// List containers carrying ALL of `label_filter` (AND semantics, via repeated
/// `--filter label=k=v`). Best-effort — empty on any error. Used by S9.5 to
/// rediscover workers the shell can't reap, and by the AC3 uniqueness probe.
#[allow(dead_code)] // S9.5 label-based worker rediscovery / AC3 uniqueness probe.
pub fn list(rt: Runtime, label_filter: &[(&str, &str)]) -> Vec<ContainerHandle> {
    let mut cmd = std::process::Command::new(rt.bin());
    cmd.args(["ps", "-a", "--format", "{{.ID}}\t{{.Names}}"]);
    for (k, v) in label_filter {
        cmd.arg("--filter");
        cmd.arg(format!("label={k}={v}"));
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let mut it = line.splitn(2, '\t');
            let id = it.next()?.trim().to_string();
            let name = it.next().unwrap_or("").trim().to_string();
            (!id.is_empty()).then_some(ContainerHandle { id, name, labels: HashMap::new() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_parsing_covers_all_values() {
        assert_eq!(Runtime::parse_selector(Some("podman")), SelectorPref::Force(Runtime::Podman));
        assert_eq!(Runtime::parse_selector(Some("docker")), SelectorPref::Force(Runtime::Docker));
        // Case + whitespace tolerant.
        assert_eq!(Runtime::parse_selector(Some("  DOCKER ")), SelectorPref::Force(Runtime::Docker));
        assert_eq!(Runtime::parse_selector(Some("none")), SelectorPref::Host);
        assert_eq!(Runtime::parse_selector(Some("host")), SelectorPref::Host);
        // Unset / empty / garbage → auto-detect.
        assert_eq!(Runtime::parse_selector(None), SelectorPref::Auto);
        assert_eq!(Runtime::parse_selector(Some("")), SelectorPref::Auto);
        assert_eq!(Runtime::parse_selector(Some("kubernetes")), SelectorPref::Auto);
    }

    #[test]
    fn resolve_selection_matches_ac1_precedence() {
        // none/host → always host, regardless of what's installed.
        assert_eq!(resolve_selection(SelectorPref::Host, true, true), Selection::Host);
        // Explicit podman → podman when present, else host (graceful fallback, AC9).
        assert_eq!(
            resolve_selection(SelectorPref::Force(Runtime::Podman), true, false),
            Selection::Container(Runtime::Podman)
        );
        assert_eq!(
            resolve_selection(SelectorPref::Force(Runtime::Podman), false, true),
            Selection::Host
        );
        // Explicit docker → docker when present, else host.
        assert_eq!(
            resolve_selection(SelectorPref::Force(Runtime::Docker), false, true),
            Selection::Container(Runtime::Docker)
        );
        assert_eq!(
            resolve_selection(SelectorPref::Force(Runtime::Docker), false, false),
            Selection::Host
        );
        // Auto stays on the host path (report-only until S9.3/S9.4 cutover), even
        // with both engines present — installing Docker can't hijack the default.
        assert_eq!(resolve_selection(SelectorPref::Auto, true, true), Selection::Host);
    }

    #[test]
    fn image_tag_is_version_pinned() {
        assert_eq!(image_tag("0.9.3"), "aish-worker:0.9.3");
        assert_eq!(image_tag("1.0.0-dev"), "aish-worker:1.0.0-dev");
    }

    #[test]
    fn container_name_is_deterministic_and_sanitized() {
        assert_eq!(container_name("sess-abc", "w_a7k3m2pQ"), "aish-sess-abc-w_a7k3m2pQ");
        // Same inputs → same name (deterministic, AC3).
        assert_eq!(
            container_name("sess-abc", "w_a7k3m2pQ"),
            container_name("sess-abc", "w_a7k3m2pQ")
        );
        // Unsafe chars collapse to '-' so the --name is always valid.
        assert_eq!(container_name("se/ss:1", "w x"), "aish-se-ss-1-w-x");
    }

    #[test]
    fn worker_labels_carry_identity_and_optional_card() {
        let l = worker_labels("w_1", "sess-a", "owner--repo", Some("card_9"), "2026-06-22T00:00:00Z");
        let map: HashMap<_, _> = l.iter().cloned().collect();
        assert_eq!(map.get("aish.schema").map(String::as_str), Some("1"));
        assert_eq!(map.get("aish.worker_id").map(String::as_str), Some("w_1"));
        assert_eq!(map.get("aish.session_id").map(String::as_str), Some("sess-a"));
        assert_eq!(map.get("aish.repo_key").map(String::as_str), Some("owner--repo"));
        assert_eq!(map.get("aish.created_at").map(String::as_str), Some("2026-06-22T00:00:00Z"));
        assert_eq!(map.get("aish.task_card_id").map(String::as_str), Some("card_9"));
        // An absent / empty card id omits the label entirely.
        let l2 = worker_labels("w_1", "sess-a", "owner--repo", None, "ts");
        assert!(!l2.iter().any(|(k, _)| k == "aish.task_card_id"));
        let l3 = worker_labels("w_1", "sess-a", "owner--repo", Some(""), "ts");
        assert!(!l3.iter().any(|(k, _)| k == "aish.task_card_id"));
    }

    #[test]
    fn default_network_is_per_os_and_runtime() {
        // Linux → host net for both engines (cheap, host-parity).
        assert_eq!(default_network(Runtime::Podman, "linux"), "host");
        assert_eq!(default_network(Runtime::Docker, "linux"), "host");
        // macOS → the engine's rootless/VM default.
        assert_eq!(default_network(Runtime::Podman, "macos"), "slirp4netns");
        assert_eq!(default_network(Runtime::Docker, "macos"), "bridge");
    }

    #[test]
    fn resource_flags_map_caps_and_omit_zero() {
        assert_eq!(
            resource_flags(4096, Some(2.0), Some(512)),
            vec!["--memory", "4096m", "--cpus", "2", "--pids-limit", "512"]
        );
        // 0 / None values omit their flag ("no limit").
        assert!(resource_flags(0, None, None).is_empty());
        assert_eq!(resource_flags(1024, None, None), vec!["--memory", "1024m"]);
        // A fractional cpu cap survives.
        assert_eq!(resource_flags(0, Some(1.5), None), vec!["--cpus", "1.5"]);
        // Zero cpus / pids are treated as "unset".
        assert!(resource_flags(0, Some(0.0), Some(0)).is_empty());
    }

    fn sample_spec() -> ContainerSpec {
        ContainerSpec {
            name: "aish-sess-w1".into(),
            image: "aish-worker:0.9.3".into(),
            argv: vec![
                "-c".into(),
                "do the thing".into(),
                "--coordinator".into(),
                "--run-id".into(),
                "w1".into(),
            ],
            labels: worker_labels("w1", "sess", "owner--repo", None, "ts"),
            state_volume_host: PathBuf::from("/home/me/.aish/workers/w1"),
            state_mount: STATE_MOUNT.into(),
            work_volume_host: Some(PathBuf::from("/home/me/proj")),
            env_file: Some(PathBuf::from("/tmp/w1.env")),
            env_inline: vec![("AISH_COORDINATOR".into(), "1".into())],
            mem_mb: 4096,
            cpus: None,
            pids_limit: Some(512),
            network: "host".into(),
            workdir: "/aish/work".into(),
        }
    }

    #[test]
    fn run_argv_is_well_formed_and_hardened() {
        let argv = run_argv(&sample_spec());
        // Starts with `run --rm --name <name>`.
        assert_eq!(&argv[0..2], &["run", "--rm"]);
        assert!(argv.windows(2).any(|w| w[0] == "--name" && w[1] == "aish-sess-w1"));
        // Carries the identity labels.
        assert!(argv.windows(2).any(|w| w[0] == "--label" && w[1] == "aish.worker_id=w1"));
        // Resource caps present (mem + pids; cpus omitted as None).
        assert!(argv.windows(2).any(|w| w[0] == "--memory" && w[1] == "4096m"));
        assert!(argv.windows(2).any(|w| w[0] == "--pids-limit" && w[1] == "512"));
        assert!(!argv.iter().any(|s| s == "--cpus"));
        // Hardened: cap-drop, never privileged.
        assert!(argv.iter().any(|s| s == "--cap-drop=ALL"));
        assert!(!argv.iter().any(|s| s == "--privileged"));
        // Secrets via env-file (kept out of argv/ps).
        assert!(argv.windows(2).any(|w| w[0] == "--env-file" && w[1] == "/tmp/w1.env"));
        // State volume mounted at the fixed path (AC4).
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/home/me/.aish/workers/w1:/aish/state"));
        // Project tree bind-mounted at the workdir.
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "-v" && w[1] == "/home/me/proj:/aish/work"));
        // Network set.
        assert!(argv.windows(2).any(|w| w[0] == "--network" && w[1] == "host"));
        // The image precedes the coordinator argv, which is preserved verbatim
        // and in order at the tail.
        let img = argv.iter().position(|s| s == "aish-worker:0.9.3").unwrap();
        assert_eq!(
            &argv[img + 1..],
            &["-c", "do the thing", "--coordinator", "--run-id", "w1"]
        );
    }

    #[test]
    fn describe_exit_names_oom_and_engine_failures() {
        assert_eq!(describe_exit(0), None);
        let oom = describe_exit(137).unwrap();
        assert!(oom.contains("OOM-killed"), "got: {oom}");
        assert!(oom.contains("AISH_WORKER_MEM_MB"), "got: {oom}");
        let eng = describe_exit(125).unwrap();
        assert!(eng.contains("container engine failed"), "got: {eng}");
        let other = describe_exit(1).unwrap();
        assert!(other.contains("exit 1"), "got: {other}");
    }

    #[test]
    fn runtime_bin_names() {
        assert_eq!(Runtime::Podman.bin(), "podman");
        assert_eq!(Runtime::Docker.bin(), "docker");
    }
}
