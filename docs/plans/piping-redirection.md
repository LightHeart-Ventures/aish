# Plan: Shell piping & I/O redirection in aish

Status: proposed
Scope: add native support for shell-style I/O operators to the aish
directly-run command path — `|` (already shipped), `>`, `>>`, `<`, `2>`,
`2>>`, `2>&1`, `1>&2`, `&>`, `&>>`, and `>/dev/null`-style sinks.

## 1. Current state (verified)

Piping is **already implemented and tested**:

- `src/pipeline.rs` — `parse()` splits a line on top-level unquoted `|` into
  argv stages and `exec()` spawns every stage with each stdout wired to the
  next stdin through a kernel pipe. Exit status is the last stage's, matching a
  POSIX shell. It ships with an **oracle differential test harness** that runs a
  corpus through both `pipeline::run` and real `bash -c` and asserts stdout +
  exit status agree byte-for-byte.
- Wired into `src/repl.rs`:
  - `dispatch()` (~L2919): if `pipeline::parse` yields stages and every stage's
    program resolves on PATH, run natively; else route to the model.
  - `route_preview()` (~L2261): the same check drives the green/"Direct"
    highlight so a pipeline is shown as runnable before Enter.
- The tokenizer `rc::tokenize_diagnosed` (src/rc.rs) treats `< > & ; ` `` ` ``
  ` * ? ( ) { } \` as unsupported metacharacters → `aish::parse::unsupported_meta`
  diagnostic → the line routes to the model. `diag.rs` already defines
  `UnsupportedMeta` and an (as-yet unused) `EmptyStage` code.

**Gap:** redirection operators (`>`, `>>`, `<`, `2>`, `2>&1`, `&>`, …) are
rejected by the tokenizer, so `sort < in > out` and `cmd 2>&1 | tee log` fall
through to the model instead of running. This plan closes that gap.

## 2. Goals / non-goals

### Goals (in scope)
| Operator | Meaning |
|----------|---------|
| `> file` | stdout → file (truncate) |
| `>> file` | stdout → file (append) |
| `< file` | stdin ← file |
| `n> file` / `n>> file` | redirect an explicit fd (`1>`, `2>`) |
| `2>&1`, `1>&2`, `n>&m` | duplicate one fd onto another |
| `&> file`, `&>> file` | stdout **and** stderr → file (bash extension) |
| `> /dev/null` etc. | any of the above with any path, incl. `/dev/null` |
| redirs **inside a pipeline** | `cat f \| grep x 2>err > out` per-stage |
| quoted / `$VAR` targets | `> "$LOG"`, `> 'my file.txt'` |

Semantics must match bash for the oracle corpus (left-to-right evaluation, last
redirection of an fd wins, `2>&1` copies the *current* target of fd 1).

### Non-goals (explicitly deferred — route to model as today)
- Control operators `&&`, `||`, `;`, background `&`.
- Command substitution `$( )` / backticks, process substitution `<( )`.
- Globbing `* ? [ ]` (aish has no glob expansion; documented elsewhere).
- Here-documents / here-strings `<<`, `<<<`.
- Redirection on shell builtins (`cd`, `export`, …) — builtins keep their
  current no-redir behavior; only external programs get redirection.
- Noclobber (`>|`), fd-close beyond `>&-`/`<&-` niceties (optional stretch).

## 3. Design

### 3.1 Data model (in `src/rc.rs`, consumed by `src/pipeline.rs`)
```rust
pub enum RedirOp { Read, Write, Append, DupOut }   // <  >  >>  >&/<&
pub struct Redir {
    pub fd: i32,                 // the fd being redirected (0/1/2/…)
    pub op: RedirOp,
    pub target: RedirTarget,     // File(path) | Fd(i32) | Both(path) | Close
}
```
A parsed stage becomes `Stage { argv: Vec<String>, redirs: Vec<Redir> }`.

### 3.2 Tokenizer (src/rc.rs) — additive, zero change to existing callers
Refactor the single scanning loop of `tokenize_diagnosed` into a private
`tokenize_core(line, lookup, redir: bool)`:
- `redir = false` → **byte-for-byte identical** to today (redir chars still
  rejected). `tokenize_diagnosed` becomes a one-line wrapper, so every current
  caller (route decisions, aliases, script mode) is unchanged.
- `redir = true` (new `tokenize_redir`) → `<`, `>`, `&`(only as `&>`), and a
  leading fd digit run are parsed into `Redir`s instead of rejected; all other
  metacharacters (`;` `` ` `` `*` `?` `(` `)` `{` `}` `\`, bare `&`, `&&`) are
  still rejected so those lines route to the model.
- Redirection parsing rules: optional fd digits immediately preceding the
  operator (`2>` → fd 2; default 1 for `>`, 0 for `<`); `>>`/`&>>` append;
  `>&`/`<&` followed by digits → `Fd(n)` dup (or `-` → close); `&>`/`>&file` →
  `Both(path)`; target is the next shell word (quote/`$`-aware, reusing the same
  word reader). Missing target → `EmptyStage`-style diagnostic (reuse the
  existing code) → route to model when unforced, caret when forced (`!`).

### 3.3 Pipeline parse/exec (src/pipeline.rs)
- `parse()` returns `Option<Vec<Stage>>`. It returns `Some` when the line is a
  **multi-stage pipeline OR any stage carries a redirection**; it returns `None`
  for a bare single command with no redirs (so the existing interactive
  `run_on_tty` path keeps owning plain foreground programs — vim/top/ssh — with
  full TTY + job control). Splitting on `|` stays in `split_top_level`; each
  segment is tokenized with `tokenize_redir`.
- `exec()` gains per-stage redirection application. After the inter-stage pipe
  wiring is set up, apply each stage's `redirs` **in order**; an explicit redir
  overrides the pipe wiring for that fd (bash semantics). Files open via
  `OpenOptions` (`read` / `write+truncate+create` / `append+create`). `2>&1`
  and friends share the *same open file description* via `File::try_clone`
  (dup(2)) so both fds point at one offset, matching bash. `&>`/`&>>` open once
  and clone for both stdout+stderr. stderr defaults to the terminal unless
  redirected.
- Single-stage-with-redir is just an N=1 pipeline; the existing capture/reap
  loop already handles N≥1 after relaxing the `>= 2` assumption in the executor.

### 3.4 Dispatch + preview (src/repl.rs)
- `dispatch()`: keep the `pipeline::parse` fast path first. Because `parse` now
  returns `Some` for single-command-with-redir too, the redirection path is
  reached automatically. Program-resolution check updated to read `stage.argv`.
- `route_preview()`: mirror the same `parse` call so a redirecting line
  highlights Direct/green when every program resolves, Model otherwise —
  keeping the pre-Enter highlight honest.

## 4. Testing
1. **rc.rs unit tests** — `tokenize_redir` over: `> f`, `>> f`, `< f`, `2> f`,
   `2>&1`, `1>&2`, `&> f`, `&>> f`, `cmd < in > out`, quoted + `$VAR` targets,
   fd-attached (`2>f` no space), and rejections (`>` no target, bare `&`, `&&`,
   `;`, backtick). Assert existing `tokenize_diagnosed` corpus is unchanged.
2. **pipeline.rs unit tests** — redir wiring: `echo hi > f` writes `hi`;
   `>>` appends; `< f` feeds stdin; `2>&1` merges; redir within a pipeline.
3. **Oracle harness extension** — add a redirection corpus to the existing
   bash-differential tests (compare final file contents + exit status against
   `bash -c`), and one deliberate-divergence guard so the harness keeps teeth.
4. `cargo test --no-default-features --locked` green.

## 5. Rollout
- Single PR (draft) on the current work branch. No feature flag needed — the
  change only *adds* accepted syntax; every line that routed to the model before
  still does unless it now parses as a redirection/pipeline.
- Update `README`/`AISH.md` "no shell syntax" note to reflect that pipes and
  redirection are now honored (control operators / globs / substitution still
  are not).

## 6. Risk & mitigation
- **Tokenizer regressions** — mitigated by keeping `tokenize_diagnosed`
  behavior byte-for-byte (core refactor + `redir=false`), plus an
  equivalence test over the existing corpus.
- **fd-dup correctness (`2>&1`)** — use `try_clone`/dup to share the file
  description; covered by the bash oracle on stdout+stderr merge cases.
- **Interactive programs losing the TTY** — plain single commands (no redir)
  still take the `run_on_tty` path untouched; only redirected/piped lines use
  the pipe executor (which is non-interactive by nature).
