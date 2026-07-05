# Orca (stablyai/orca) — Feature Analysis & Cherry-Picks for aish

**Source:** https://github.com/stablyai/orca — "The AI Orchestrator for 100x builders."
An Electron **ADE** (Agent Development Environment) for running a *fleet of parallel
CLI coding agents* (Claude Code, Codex, OpenCode, Cursor, Pi, …), each in its own git
worktree, tracked in one place. Desktop + mobile companion. ~12k★, TypeScript.

**License:** **MIT** — unlike AGPL herdr (FR-331), we may reference Orca's source
directly with attribution. Everything below is still a *pattern/design* cherry-pick,
reimplemented natively in Rust against aish's existing plumbing — we are not vendoring
an Electron app.

> **North-star difference:** Orca is a **GUI ADE** (Chromium windows, WebGL terminals,
> VS Code editor, mobile app). aish **is the terminal** — headless, single-user,
> tool-driven, with background coordinators + worktree isolation + `:alert`/`set_alert`
> + SecondStatusLine + plugins + MCP. So we cherry-pick the *orchestration patterns*
> that map onto a terminal-native model and explicitly reject the GUI-bound ones.

---

## Orca's full feature surface (decoded from README + repo)

| # | Feature | What it does | aish fit |
|---|---------|--------------|----------|
| 1 | **Parallel Worktrees** | Fan ONE prompt across N agents, each in its own isolated worktree; compare results, merge the winner | ✅ **PICK** — aish has worktree+coordinator plumbing but no race/compare/merge-winner command |
| 2 | **Account switcher & usage tracking** | See Claude/Codex usage + rate-limit resets; hot-swap accounts without re-logging-in | ✅ **PICK** — aish is single-credential today |
| 3 | **GitHub & Linear native** | Browse PRs/issues/boards in-app; open a worktree from any task | ✅ **PICK** (partial) — the *open-worktree-from-task* verb, terminal-native |
| 4 | **Annotate AI Diffs** | Drop line comments on any diff and ship them back to the agent | ✅ **PICK** (partial) — terminal diff-review that folds comments into `tell` |
| 5 | **Mobile companion / Notifications** | Get notified when an agent finishes/needs attention; steer + send follow-ups from anywhere; unread state | ✅ **PICK** (transport only) — remote push bridge for done/blocked, minus the GUI |
| 6 | **Terminal Splits** | Ghostty-class WebGL terminals, infinite splits, restart-surviving scrollback | ❌ REJECT — aish *is* the terminal; splits are the host terminal's job |
| 7 | **Design Mode** | Click a UI element in a Chromium window → send HTML/CSS + screenshot to prompt | ❌ REJECT — Electron/Chromium bound |
| 8 | **Drag files to agents** | VS Code editor w/ autosave; drag files/images into a prompt | ❌ REJECT — GUI; aish already takes paths in prompts |
| 9 | **Rich repo previews** | Preview Markdown/images/PDFs/docs in-workspace | ❌ REJECT — GUI |
| 10 | **Computer Use** | Agents operate desktop apps / visible UI | ❌ REJECT — huge, out of scope |
| 11 | **SSH Worktrees** | Run agents on a remote box w/ full editing, git, terminals; auto-reconnect + port-forward | ⏸ DEFER — large; a future remote-execution track |
| 12 | **Orca CLI** | Agents drive Orca: `orca worktree create`, `snapshot`, `click`, `fill` | ➖ N/A — aish already *is* the scriptable CLI; agents drive via tools |
| 13 | **Quick open** | Fuzzy palette across worktrees, files, agents, commands, repo context | ⏸ STRETCH — partial overlap w/ `background_status`; low priority |

---

## The cherry-picks (terminal-native)

### 1. `:fanout` — prompt-race + compare + merge-winner  *(HIGH)*
Orca's headline. aish already isolates every background job in a worktree and can fan
work out (`run_in_background`, batch tier) — but there is **no first-class "run ONE
prompt across N agents, then compare and merge the winner."** Today a human hand-spawns
N coordinators and eyeballs the branches.

- `:fanout <n> <prompt>` → spawn N coordinators on the **same** task, each in its own
  worktree/branch (`fanout/<slug>-{1..n}`), concurrency-capped.
- `:fanout status` → live rollup of the N candidates (reuses `background_status`).
- `:fanout compare` → diff-matrix / per-candidate `git diff --stat` + summary.
- `:fanout pick <k>` → merge the winning branch, discard the rest, GC worktrees.

Maps onto: `coordinator.rs` dispatch, worktree isolation, `background_status`, git.

### 2. `:account` — multi-credential switcher + rate-limit awareness  *(HIGH)*
aish resolves a **single** `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN` (or Grok
login) from env/`~/.aishrc`. Orca lets you keep **multiple accounts** and hot-swap to
dodge rate limits. `${profile:KEY}` already exists for MCP/plugins — generalize it to
the main backend.

- Named accounts in `~/.aish/credentials` (`[account:work]`, `[account:personal]`, …),
  plus an active-account pointer.
- `:account list | current | use <name> | add <name>` — hot-swap **without restart**
  (re-point the backend credential, no re-login).
- `:account current` surfaces the active account's **rate-limit reset** window.
- **Complements FR-326** (ccquota subscription-usage badge) — that FR *displays* burn;
  this one *switches* the account and shows the reset. Cross-reference, don't dup.

### 3. `:worktree from-pr | from-issue | from-card`  *(MED-HIGH)*
Orca opens a worktree directly from a GitHub/Linear task. aish has `gh` + the Atum MCP
board but no one-shot "task → isolated worktree (+ optional coordinator)."

- `:worktree from-pr <n>` → worktree checked out on the PR's head branch, ready to review/fix.
- `:worktree from-issue <n>` → fresh `feat/iss-<n>` worktree + issue body preloaded as context.
- `:worktree from-card <TASK-###>` → Atum card → worktree + coordinator with the card's
  spec/acceptance-criteria preloaded.

### 4. `:review <job>` — terminal diff-review with line annotations → `tell`  *(MED)*
Orca's "Annotate AI Diffs," terminal-native. Render a finished coordinator's diff, let
the operator attach line-scoped comments, and ship the collected notes back to a
follow-up coordinator via the existing `tell` channel (or a fresh `run_in_background`).

### 5. Coordinator done/blocked **push-notification bridge**  *(MED)*
The valuable half of Orca's mobile companion — *"know when an agent finishes or needs
attention, from anywhere"* — without building a GUI/mobile app. A plugin hook fires an
outbound push (ntfy / Pushover / generic webhook) on coordinator `done`/`blocked`.
**Complements FR-331** (fleet rollup / "who needs you") and `set_alert` — this is the
remote *transport*, those are the local classification.

---

## Explicitly out of scope
GUI-bound features (#6–#10, #13): terminal splits/WebGL, Design Mode, drag-to-agent,
rich previews, Computer Use, fuzzy quick-open palette. SSH remote worktrees (#11)
deferred as a separate large track. Orca CLI (#12) is already aish's native model.

## Dedup ledger
- **FR-331** (herdr Fleet Agent-State Awareness) — status classification + rollup +
  "who needs you." Cherry-pick #5 (notification bridge) is the remote transport that
  *rides on* FR-331's classification, not a re-implementation.
- **FR-326** (ccquota subscription-usage badge) — *displays* Claude Code burn. Cherry-pick
  #2 (`:account`) *switches* accounts and shows rate-limit resets. Complementary.
