# Host-brokered sibling spawn — security model

Companion to `docs/host-brokered-sibling-spawn.md`. That document explains *how*
the flat worker topology works; this one explains **what the host trusts, what it
refuses, and what is still open.**

Code: `src/spawn_broker.rs` (transport), `src/spawn_broker_policy.rs` (policy),
`src/spawn_broker_host.rs` (accept loop), `src/spawn_broker_registry.rs`
(read-back registration).

---

## 1. The trust boundary

A nested coordinator asks for a sibling by **writing a file**:

```
<worker-state-dir>/spawn-requests/spawn-req-<id>.json   ← written by the WORKER
        ▲ bind-mounted at /aish/state inside that worker only
```

The host polls that spool, claims a request, and launches a container. So the
boundary is stark: **a `SpawnRequest` is attacker-controlled input that causes the
host to execute a process.** Every field in it is self-asserted by the requester.

The pre-hardening implementation acted on those fields directly. That gave a
compromised (or merely looping) coordinator several free wins:

| Field | Blind-trust consequence |
|---|---|
| `spawn_budget` | claim `u32::MAX` → the fork-bomb backstop never fires |
| `requested_by_worker` | attribute your spawns to another worker → the audit trail lies |
| `cwd` | `..` components → host operates outside the intended repo |
| `task` | unbounded blob → copied into argv, logs, and a DB row |
| `backend` / `base` | unexpected values reach the argv builder |
| `created_at_unix` | a day-old request replays against a changed tree |
| *(rate)* | nothing bounded how many requests per second a worker could emit |
| *(mode)* | spool files inherited umask → prompts readable by other local accounts |
| *(record)* | no durable answer to "who spawned what, when" |

---

## 2. What the host now enforces

### 2.1 Provenance is derived, never trusted — `worker_id_from_spool_path`

Each worker gets **its own** state volume (`state_volume_host` → `/aish/state`).
A worker can therefore only write into *its own* spool. That makes the arrival
path an unforgeable capability, and the host treats it as the authoritative
identity:

* the requester is read from the spool path, not the payload;
* a payload that *claims a different worker* is rejected as
  `provenance_mismatch` (it is not silently relabeled — a lie is signal);
* the request handed to the launcher has `requested_by_worker` **rewritten** to
  the derived identity, so registry rows and logs record who really asked;
* the disputed claim is preserved in the audit record (`claimed_requester`) for
  forensics.

This is authentication by topology. No shared secret is involved — and
deliberately so: a shared HMAC key mounted into every sibling would let any
sibling sign as any other, which is exactly the property we needed to prevent.

### 2.2 Field validation — `RequestPolicy::validate`

| Rule | Reject code |
|---|---|
| `schema_version` must match the host | `schema_version` |
| `request_id` non-empty, ≤ 64 chars, `[A-Za-z0-9._-]` only | `bad_request_id` |
| `task` non-blank | `empty_task` |
| `task` ≤ 64 KiB | `task_too_large` |
| `cwd` ≤ 4096 bytes | `cwd_too_large` |
| `cwd` absolute | `cwd_not_absolute` |
| `cwd` free of `..` components | `cwd_traversal` |
| `backend` ∈ {`claude`, `grok`} | `unknown_backend` |
| `base` ∈ {`main`, `head`} | `unknown_base` |
| `spawn_budget` ≤ host ceiling | `budget_too_high` |
| `created_at_unix` ≤ now + 300s | `future_timestamp` |
| age ≤ 24h | `stale` |
| claimed requester matches the arrival path | `provenance_mismatch` |

The `request_id` charset rule matters beyond tidiness: the id is interpolated
into the spool filename, so `/` or `..` in it would be a path-traversal write.

### 2.3 Rate limiting — `RateLimiter`

The budget gate bounds spawn **depth**. Nothing bounded spawn **rate**, which is
the axis an actual runaway hits: a coordinator in a retry loop can write spool
files as fast as the filesystem allows, and each one costs the host a container.

A sliding window (default **8 spawns / 60 s**, per requester) caps that. The
window is keyed on the *derived* requester, so a worker cannot dodge its own
limit by claiming to be someone else. A refused attempt is **not** recorded as a
hit, so a hammering requester does not extend its own penalty window.

State lives in the long-lived host loop's `BrokerGuard` and is garbage-collected
each tick, so many short-lived requesters do not leak memory.

### 2.4 Denial-of-service containment

* **Bounded reads** — `claim` refuses a spool file over 256 KiB by `stat`, before
  allocating. An oversized blob costs a syscall, not memory.
* **No poison pill** — a malformed/oversized/unparsable request is recorded as a
  rejection and the batch **continues**. Previously an unparsable file propagated
  its error out of the accept loop, so one junk file blocked *every other
  coordinator's* spawns. There is a regression test for exactly this
  (`malformed_request_does_not_wedge_the_batch`).
* **Discard, never requeue** — a rejected request is deleted. Requeueing would
  let a hostile requester grow the spool without bound and re-cost the host on
  every tick. The audit record is the durable trace.

### 2.5 Confidentiality of the spool

A task prompt routinely contains material that should not be readable by every
local account.

* spool directory → `0700`, request files → `0600`;
* the `tmp` file is clamped **before** the atomic rename, so there is no window
  in which a half-written request is world-readable;
* a pre-existing loose (`0777`) spool dir is tightened on the next write;
* process umask is therefore not load-bearing.

### 2.6 Audit trail — `audit.jsonl`

One append-only JSON line per decision — `launched`, `refused_budget`,
`rejected`, `failed` — in the spool dir, created `0600` (mode set at `open` time,
not after). Each record carries the derived requester, the disputed claim if
any, the session id, `cwd`, `isolate`/`base`, the stamped sibling budget, the
reason code, and a **redacted, 200-byte** task preview.

Redaction masks common credential shapes (`password=`, `token=`, `api_key:`,
`Bearer …`) before anything is written. It is defence in depth for the *log*; the
authoritative protection for the *payload* is the owner-only file mode.

---

## 3. Known limitations (deliberately not claimed as fixed)

### 3.1 No privilege separation between siblings — **open**

Siblings run as the host aish's uid. Provenance binding makes *attribution*
trustworthy; it does **not** sandbox a sibling. A compromised sibling has the
same filesystem and daemon reach as any other worker the host launched.

Closing this needs a container-layer change — per-worker uids or user namespaces,
plus a state-volume ownership scheme — not a policy-layer one. Until then:
**treat a sibling as equally privileged to its requester**, and do not rely on
the broker as an isolation boundary between mutually-distrusting tasks.

### 3.2 No inter-sibling network isolation — **open**

Siblings share the host daemon's default bridge network, so any sibling can
reach any other's listening ports. Mitigation when it matters: a per-run
user-defined network, or `--network none` for workers that need no egress.
Neither is wired today.

### 3.3 Secrets in the task text — **partially mitigated**

Secrets pasted into a task still land in the spool payload (protected by file
mode, not by scrubbing) and in the sibling's argv. Audit-log redaction is
best-effort pattern matching, not a guarantee. The durable fix is to keep
credentials in the credential store and reference them
(`${profile:KEY}`) rather than inlining them in a prompt.

### 3.4 Time is wall-clock

Staleness and rate limiting use wall-clock seconds. A host whose clock jumps
backwards can transiently accept a request it would otherwise call stale. The
300 s skew tolerance is the only allowance; monotonic sequencing would need a
host-side counter in the spool.

---

## 4. Threat-model summary

| Threat | Control | Status |
|---|---|---|
| Worker A impersonates worker B | path-derived provenance + reject on mismatch | ✅ fixed |
| Forged `spawn_budget` defeats fork-bomb backstop | budget ceiling in `validate` | ✅ fixed |
| Path traversal via `cwd` / `request_id` | absolute + no-`..` + id charset | ✅ fixed |
| Unbounded task / spool blob | 64 KiB task cap, 256 KiB file cap, stat-before-read | ✅ fixed |
| Spawn flood | per-requester sliding window | ✅ fixed |
| One junk file blocks all spawns | per-request error isolation | ✅ fixed |
| Replay of a stale request | skew + max-age bounds | ✅ fixed |
| Prompt readable by other local users | `0700`/`0600`, clamped pre-rename | ✅ fixed |
| No forensic record | append-only redacted `audit.jsonl` | ✅ fixed |
| Sibling privilege escalation | — | ⚠️ open (§3.1) |
| Inter-sibling network reach | — | ⚠️ open (§3.2) |
| Secrets inlined in a task | redaction (log only) + file mode | ⚠️ partial (§3.3) |

---

## 5. Operator notes

* **Leave `derive_provenance` on.** It is the only thing standing between the
  audit trail and a worker that lies about who it is.
* `BrokerGuard::permissive()` keeps validation + provenance but drops rate
  limiting and audit. It exists for tests and one-shot drains — not for
  production.
* A leftover `.claimed` file is **inert**: `claim` already moved it out of the
  pending set, so it is never re-spawned. Retaining it after a launcher failure
  is intentional (forensics).
* To review activity: `audit.jsonl` is one JSON object per line in each worker's
  `spawn-requests/` directory — greppable by `decision`, `reason_code`, or
  `requester`.
