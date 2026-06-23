# Session-scoped background jobs — design

Status: **design + skeleton** (this PR). Full filtering behaviour lands in a
follow-up — see `docs/session-scoped-jobs-implementation.md`.

## Problem

`background_status` answers "what's running?" by listing **every** background
job the host can see: this session's in-memory coordinators, plus every
session's durable coordinator runs and Anthropic batches (read from the shared
`aish.db`). That cross-session firehose is correct for "show me everything" but
wrong for the common case — a user typing `status` almost always means *my*
jobs, in *this* shell, not the jobs another aish process on the same machine
left running.

We want session-scoped queries:

| Query                     | Scope shown                                              |
|---------------------------|----------------------------------------------------------|
| `status`                  | jobs owned by the current session (the eventual default) |
| `status of all sessions`  | every job, every session (today's behaviour)             |
| `status of <repo>`        | jobs whose repo-key matches (e.g. `status of aish`)      |
| `status of <job-id>`      | one job by id / id-prefix (e.g. `status of w_a7k3m2pQ`)  |

## What already exists (no change needed)

A surprising amount of the machinery is already in the tree — this design
*reuses* it rather than rebuilding it:

| Capability | Where | Notes |
|---|---|---|
| Stable per-session id | `Session::session_id` (`session.rs`) | UUIDv4 minted once in `Session::new()` |
| Friendly session name | `Session::name` (`:rename`) | display label, optional |
| Job → session tagging | `coordinator_runs.session_id`, `batch_jobs.session_id` (`db.rs`) | nullable columns, written at insert |
| Coordinator adopts launching id | `WorkerSpec::launch_session_id` → `AISH_LAUNCH_SESSION_ID` → `main.rs` re-adopt | a coordinator's durable rows attribute to the session that asked |
| "Owner" column | `background_status` (`tools.rs`) | already renders `you` vs another session's name/short-id |

So the **session id, its propagation across the parent→coordinator boundary,
and per-job ownership tagging are all done.** The two genuine gaps are:

1. **No `AISH_SESSION_ID` in the child environment.** Only coordinators get an
   id (via `AISH_LAUNCH_SESSION_ID`). A plain `run_program`/`run_in_background`
   child can't read the session id to tag anything it does. → *This PR exports
   it.*
2. **No scope filter on `background_status`.** The tool takes no arguments and
   always lists everything. → *This PR adds the `scope` parameter + the pure
   filtering logic (`src/scope.rs`); wiring the new default is the follow-up.*

## Session ID

- **Format:** keep the existing **UUIDv4** (`Session::session_id`). It is
  already collision-free across concurrent shells and already what every
  durable row is keyed on — inventing a timestamp/hybrid scheme now would
  orphan every existing tagged row. The short 8-char form
  (`crate::batch::short_id`) is used purely for *display*.
- **Generation:** at **shell startup**, once, in `Session::new()` — unchanged.
  Per-job ids stay separate (`w_########` worker ids, `run_*` coordinator ids);
  the session id groups them by owner.
- **Export:** `main.rs` exports `AISH_SESSION_ID` into the session env (via
  `Session::set_var`) so every spawned child inherits it. A coordinator
  re-adopts the **launching** session's id (`AISH_LAUNCH_SESSION_ID`, set by
  `worker_command`) and re-exports it, so a coordinator and the jobs *it*
  spawns all tag back to the human's original session — not the coordinator's
  throwaway uuid.

```
interactive shell (sid=A)
  ├── AISH_SESSION_ID=A ──> run_program child         (tags as A)
  └── run_in_background ──> coordinator
                              AISH_LAUNCH_SESSION_ID=A
                              ├── re-adopts sid=A, re-exports AISH_SESSION_ID=A
                              └── its batches/children  (tag as A, not the child uuid)
```

## Filter semantics (`src/scope.rs`)

A single pure enum + two pure functions, unit-tested without any I/O:

```rust
pub enum JobScope { Session, All, Repo(String), Job(String) }

JobScope::parse(raw: Option<&str>) -> JobScope   // "status of X" → scope
JobScope::matches(&self, job: &JobRef, current_session_id: &str) -> bool
```

`parse` maps the free-text the agent extracts from "status of X":

| Input (case-insensitive)                          | Scope            |
|---------------------------------------------------|------------------|
| *(absent)* / `""` / `session` / `mine` / `this`   | `Session`        |
| `all` / `all sessions` / `everything` / `*` / `any` | `All`          |
| `job:<id>` or a token shaped like a job id        | `Job(<id>)`      |
| `repo:<key>` / any other token                    | `Repo(<token>)`  |

`matches` decides per job, given a `JobRef { owner_session_id, repo_key, id }`:

- **Session** — `owner_session_id == Some(current)`.
- **All** — always true.
- **Repo(k)** — `repo_key` equals/contains `k` (case-insensitive).
- **Job(q)** — `id == q || id.starts_with(q)`.

### Backward compatibility (legacy jobs)

Rows written before ownership tracking have `session_id = NULL`. Under
**Session** scope they are **excluded** (they aren't provably "yours"); under
**All** they are **included** and render `owner = —`, exactly as today. No
migration, no row rewrite — a null owner simply never matches the session
filter. `Repo` scope likewise treats a missing repo-key as "no match" so legacy
rows never falsely appear under a repo query.

## Why default stays `All` in *this* PR

The skeleton wires the parameter and ships the pure logic but **leaves the
default at `All`** (today's behaviour) so the PR is a pure, reviewable
add — zero behaviour change for anyone who doesn't pass `scope`. Flipping the
default to `Session` (plus the system-prompt nudge that teaches the model to
pass `scope:"all"` only when the user says "all sessions") is the first task of
the follow-up PR, gated behind review of this design.

## Out of scope here (follow-up)

- `repo_key` column on `coordinator_runs` / `batch_jobs` (needed for `Repo`
  filtering against durable rows — the skeleton stubs it).
- Flipping the `background_status` default to `Session`.
- System-prompt guidance for scope extraction.
- `:status`/`:workers` REPL meta-command honouring the same scope grammar.
