# Multi-Account Credential Model — Design

- **Task:** TASK-414 (FR-336 #2)
- **Status:** Design (implementation deferred to TASK-415; rate-limit awareness to TASK-416)
- **Source:** `docs/orca-analysis.md` §2 (Orca account-switcher)
- **Deliverable of this doc:** the credential file schema, active-account state, hot-swap
  semantics, precedence, and the `:account` command surface — enough for TASK-415 to implement.

## 1. Problem

aish resolves the primary-backend credential (`CLAUDE_CODE_OAUTH_TOKEN` /
`ANTHROPIC_API_KEY`) **once**, from the environment, at process start. An operator who
holds more than one Claude identity — e.g. a work subscription and a personal one, or a
rate-limited account and a fresh one — must edit `~/.aishrc`/env and `:restart` to switch.

The `${profile:KEY}` mechanism (see `mcp.rs` / `plugin_auth.rs`) already loads named
credential sections from `~/.aish/credentials` and hands them to MCP servers and plugins
without leaking raw secrets into the conversation. This design **generalizes that same
named-section machinery to the primary backend credential** and adds a live "active
account" pointer so switching needs no restart.

## 2. Goals / Non-goals

**Goals**
- Named accounts in `~/.aish/credentials` (`[account:work]`, `[account:personal]`, …).
- A durable **active-account** pointer.
- **Hot-swap:** `:account use <name>` changes the credential used by *subsequent* turns with
  no process restart.
- **Backward-compatible:** a setup with only `ANTHROPIC_API_KEY`/`CLAUDE_CODE_OAUTH_TOKEN`
  in the environment and no account sections behaves exactly as it does today.
- **Secret-safe:** token values are never rendered into transcripts, logs, or `:account`
  output (masked tails only).

**Non-goals**
- Implementing the `:account list|current|use|add` command (TASK-415).
- Rate-limit / usage-reset awareness surfaced in `:account current` (TASK-416).
- Team/multi-tenant shared credential vaults.

## 3. Credential file schema

`~/.aish/credentials` is the existing INI-style file already parsed for `[profile]`
sections. Accounts add a parallel `account:` namespace plus one `[active]` stanza:

```ini
# Named accounts — each carries at least one backend token.
[account:work]
claude_code_oauth_token = sk-ant-oat01-XXXXXXXX

[account:personal]
anthropic_api_key = sk-ant-api03-YYYYYYYY

# Optional future backend key alongside the Claude token:
# grok_api_key = xai-ZZZZ

# Durable pointer to the currently-selected account.
[active]
account = work
```

Rules:
- Section prefix **`account:`** is loaded by the same section parser that handles
  `[profile]` — new namespace, no new file, no new parser.
- Recognized keys per account: `anthropic_api_key`, `claude_code_oauth_token`, and
  (reserved) `grok_api_key`. **At least one** backend token is required for a valid account.
- `[active] account = <name>` names the durable active account. It is the single source of
  truth persisted across restarts. Absent ⇒ no persisted selection (see precedence).

## 4. Active-account state & precedence

The primary-backend credential is resolved by `resolve_active_credential()` evaluated **at
each turn boundary** (not cached once at init). Precedence, highest first:

1. **Session override** — set by `:account use <name>` during this session (in-memory).
   Wins for the rest of the session unless changed again.
2. **`[active] account`** pointer in `~/.aish/credentials`.
3. **Legacy environment** — `CLAUDE_CODE_OAUTH_TOKEN`, then `ANTHROPIC_API_KEY` (today's
   exact path, unchanged).

If **no `[account:*]` sections exist**, steps 1–2 are skipped entirely and resolution is
byte-for-byte the current behavior (Goal: backward-compat).

`:account use <name>` may optionally also persist `[active] account = <name>` so the choice
survives a restart; the in-memory override always takes effect immediately regardless.

## 5. Hot-swap semantics

Today the backend client is built once with a credential captured at startup. This design
moves the credential read behind `resolve_active_credential()`:

- At the **start of each turn**, the backend client credential is (re)resolved. If it
  changed since the last turn, the client is rebound with the new token.
- An **in-flight turn is never disturbed** — it keeps the client it started with. The swap
  takes effect on the *next* turn boundary.
- No `:restart`. This is the core behavioral difference from the status quo.

Concurrency note (feeds FR-336 #1 fan-out): because resolution is per-turn and parametric,
distinct coordinators can each pin a distinct account, enabling N-way fan-out across
accounts without cross-talk.

## 6. `:account` command surface (implemented in TASK-415)

| Command | Behavior |
|---|---|
| `:account list` | Show account names + which is active; token values masked (tail only). |
| `:account current` | Show the resolved active account and its backend/source (session / active-pointer / legacy-env). |
| `:account use <name>` | Set the session override (and optionally persist `[active]`); hot-swaps next turn. |
| `:account add <name>` | Interactively/append a new `[account:<name>]` section (secret entered out-of-band, never echoed). |

## 7. Backward-compatibility matrix

| Setup | Resolved credential |
|---|---|
| env only, no `~/.aish/credentials` | legacy env (unchanged) |
| `~/.aish/credentials` present, **no** `[account:*]` | legacy env (unchanged) |
| `[account:*]` **and** `[active] account = X` | account X's token |
| `[account:*]` but **no** `[active]`, no session override | falls through to legacy env |
| `:account use Y` issued this session | account Y (session override wins over `[active]`) |

## 8. Secret hygiene

- Token values reuse the `${profile:KEY}`-style indirection: raw secrets never enter the
  conversation, transcripts, or logs.
- `:account list` / `:account current` print names, sources, and **masked tails**
  (e.g. `sk-ant-…YYYY`) only.
- `:account add` accepts the secret out-of-band (prompt not echoed) and writes it to
  `~/.aish/credentials` with the same file permissions the profile loader already expects.

## 9. Modules touched at implementation time (TASK-415)

- **`mcp.rs` / `plugin_auth.rs`** — generalize the existing profile/section loader to also
  surface `account:` sections and the `[active]` pointer.
- **Backend client init** — replace the one-shot env read with a call to
  `resolve_active_credential()` at the turn boundary; rebind the client on change.
- **`:account` command handler** — new command implementing the surface in §6.

## 10. TASK-415 acceptance criteria (derived from this design)

1. Precedence order in §4 holds (unit tests: session > active-pointer > legacy env).
2. Empty account set ⇒ resolution identical to pre-change (regression test).
3. `:account use` hot-swaps with no restart (integration: two accounts, switch, next turn
   uses the new token).
4. No token value ever appears in `:account` output or logs (assertion on masked rendering).
