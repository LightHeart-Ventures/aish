# Emoji-prefixed output format (`:output on`)

When background-coordinator activity is streamed to the terminal via
`:output on` (alias `:wo`), each forwarded tool line is prefixed with a
**source glyph** that tells you at a glance *where* the tool ran:

| Glyph | Source | Meaning |
| ----- | ------ | ------- |
| ⚙️ | **local execution** | A tool aish runs in-process on this host — `run_program`, `run_interactive`, `read_file`, `write_file`, `edit_file`, `list_dir`, `change_dir`, `remember`, `recall`, … |
| 🔧 | **MCP tool call** | A tool served by a connected MCP server — catalog names prefixed `mcp__<server>__<tool>`, or the bare `mcp_…` / `atum_…` shorthands |

## Examples

A streamed coordinator activity line is the existing
`<status> 🔧 <descriptor>` shape; the source glyph is **prepended** to it:

```
⚙️ ✓ 🔧 read /etc/hosts
⚙️ ✓ 🔧 run_program ./scripts/build.sh
🔧 ✓ 🔧 mcp__atum__atum_list_project_board
🔧 ✗ 🔧 atum_get_project_task
```

Conceptually, the source glyph travels *before the tool name*:

```
⚙️ run_program(…)            ← local execution
🔧 atum_list_project_board(…) ← MCP tool call
```

## Selection rule

The classifier is the pure helper `source_emoji(tool_name)` in
[`src/worker.rs`](../src/worker.rs):

```rust
fn source_emoji(tool_name: &str) -> &'static str {
    let name = tool_name.trim();
    if name.starts_with("mcp__") || name.starts_with("mcp_") || name.starts_with("atum_") {
        "🔧" // MCP tool call
    } else {
        "⚙️" // local execution
    }
}
```

* Any name with an MCP prefix (`mcp__`, `mcp_`, `atum_`) → 🔧.
* Everything else → ⚙️ (the safe default — an unknown or empty token reads as
  local).

`⚙️` is the gear plus a VS16 emoji-presentation selector (`⚙\u{fe0f}`), matching
the gear already used for the `escalate` hand-off elsewhere in the activity
stream.

## Where it is applied

The prefix is added in the **`:output on` forwarding path only**
(`forward_decision` → `decorate_activity_source` in `src/worker.rs`). That
function short-circuits to "forward nothing" when the toggle is off, so:

* **Only `:output on` is affected.** With output off (the default) a background
  coordinator stays quiet and no emoji is emitted.
* **The existing format is preserved.** The glyph is *purely prepended*; the
  original `<status> 🔧 <descriptor>` line is forwarded verbatim after it.
* **Backward compatible.** The emoji is visual sugar — it carries no data and is
  never parsed downstream. The prompt-badge pulse classifier
  (`classify_event`) reads the `✓`/`✗` status glyph and the wrench, both of
  which are untouched.
* **Sync and async alike.** Both the asynchronous background worker
  (`run_worker` → `stream_stderr`) and the synchronous goal-loop runner
  (`run_once` → `stream_stderr`) route every line through `forward_decision`,
  so the decoration applies uniformly to both.

A line that already carries its own source marker (the `escalate` ⚙️ gear,
which has no wrench) is left untouched so it is never double-marked.

## Source extraction from a forwarded line

A streamed line carries the *descriptor* (e.g. `read /etc/hosts`), not the raw
tool name. `activity_tool_token` recovers the identifying token as the first
word after the 🔧 wrench:

* For an MCP tool the coordinator emits the raw catalog name as the descriptor
  (`mcp__atum__…`), so the token classifies as MCP.
* For a local tool the descriptor is a human verb (`read`, `write`, a program
  name) with no MCP prefix, so it classifies as local.

This keeps the classification accurate without threading the raw tool name
through the parent/child process boundary.
