# Host-brokered sibling spawn

Replaces the docker-in-docker / **Sysbox** nested-runtime path (deleted) with a
flat, star-topology worker model: a nested coordinator that wants to run *its*
workers in containers does **not** spawn them from inside — it emits a
spawn-request that the **host** `aish` services with a plain `docker run`
**sibling** off its own normal daemon.

## Why we deleted Sysbox

The nested-runtime path existed for exactly one case: a coordinator running
*inside* a container that wanted each child to be *its own* container. Achieving
that required either the `sysbox-runc` OCI runtime (custom runtime, Docker-only,
must be installed + registered with dockerd, needs cap-drop carve-outs, no
macOS) or bind-mounting `/var/run/docker.sock` (hands the container host root).
Both are heavy, and — critically — grandchild containers are **invisible** to
the host's cleanup/observability: `container::list`/`rm`/`forget_container`
filter on the host daemon's `aish.worker_id` label, which nested grandchildren
under a private inner daemon never carry.

## The flat design

```
interactive aish (HOST)  ── owns the real docker daemon + all secrets
  │  docker run  ─────────────►  worker #1  (sibling, labelled aish.worker_id)
  │                                  │  run_in_background  (nested)
  │                                  ▼
  │                          writes spawn-req-<id>.json to the shared
  │                          state spool (/aish/state/spawn-requests/)
  │  ◄──────────────  host poller claims the request, checks budget
  └─ docker run  ─────────────►  worker #2  (SIBLING of #1, not a child)
```

Every worker is a first-class sibling under one daemon. Wins:

* **No nesting runtime, no socket mount, no cap-drop carve-outs, macOS works.**
* **Cleanup/observability already work** — every sibling carries the host's
  `aish.worker_id` label, so the existing `container::list`/`rm`/`forget_container`
  see them uniformly.
* **Reuses the argv single-source-of-truth** (`worker::coordinator_argv`): the
  host rebuilds the sibling command from the same argv; the event only carries
  the non-secret `SpawnRequest`.
* **Secrets never travel in the event** — the host already holds them (it
  launched worker #1) and injects from its own env.

## Transport (v1: spool directory)

The state volume is *already* mounted into every worker (`state_volume_host` →
`/aish/state`). The broker uses a sub-directory of it:

| Step | Actor | Mechanism |
|---|---|---|
| emit | nested worker | `write_request` → atomic tmp+rename `spawn-requests/spawn-req-<id>.json` |
| discover | host poller | `list_pending` (poll or inotify) |
| claim | host poller | `claim` renames `.json` → `.json.claimed` (mutual exclusion; crash-restart never double-spawns) |
| gate | host poller | `sibling_budget(req.spawn_budget)` — refuse at 0, else stamp `budget-1` |
| launch | host | `worker::coordinator_argv` + `container::run_argv` → `docker run` |
| gc | host | `discard_claimed` (best effort; a leftover `.claimed` is inert) |

It is nearly free, durable, and restart-survivable. A Unix-socket transport
(lower latency, interactive tier) and the bidirectional webhook broker
(multi-host) are drop-in replacements for the same `SpawnRequest` payload — the
event abstraction makes those a transport swap, not a redesign.

## Budget gate (fork-bomb backstop)

The existing `AISH_SPAWN_BUDGET` guard (default 3) moves from the fork site to
the host accept loop. `SpawnRequest.spawn_budget` carries the requester's
remaining budget; `sibling_budget` returns `None` (REFUSE) at 0, else
`Some(budget-1)` to stamp on the sibling. Same guarantee, enforced at the
broker.

## Result read-back

`run_in_background` today returns a job in the *requesting process's* in-memory
`worker_jobs`; a host-forked sibling wouldn't appear there. The follow-up
registers each sibling in the durable, session-scoped `coordinator_store`
(keyed on `launch_session_id`, already threaded through `SpawnRequest`), and the
worker's `background_status`/`job_output` fall back to `coordinator_store` for
siblings spawned on its behalf. `coordinator_store` is already durable +
restart-re-attaching, so this is wiring, not new infra.

## Caveats

* **Same-host only.** Siblings land on the host daemon (Sysbox was effectively
  same-host too). Multi-host graduates to the webhook-broker transport — a
  transport swap behind the same payload.
* **Host `aish` must be alive** to service spawns — but it already owns the
  top-level session, and restart-survival via `coordinator_store` already
  re-attaches live runs.

## Wiring status

- ✅ Sysbox path deleted (`src/container.rs` runtime override, `Dockerfile.worker`
  inner-dockerd layer + entrypoint wrapper, `docs/sysbox.md`,
  `scripts/install-sysbox.sh`, `scripts/worker-entrypoint.sh`).
- ✅ `src/spawn_broker.rs` — transport + protocol + budget gate, unit-tested.
- ⏳ Follow-up: nested-worker emit site, host accept loop, `coordinator_store`
  sibling registration + status read-back.
