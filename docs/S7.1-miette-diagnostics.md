# S7.1 / TASK-139 — PRD: miette-backed diagnostics (spans + codes + hints)

**Card:** TASK-139 (`card_8aa65edb8b5a`) · sprint `sprint_ceae1c2263d6` · 5 pts · high
**Engineering spec:** `docs/S7.1-miette-diagnostics-eng-spec.md`
**AC (card):** *a routing/parse error renders with a caret + code + hint*

This is the design rationale. The companion engineering spec turns it into a
build plan (module layout, signatures, sequencing, test matrix); this document
is the *why* and the *what*, and the spec assumes it.

---

## 1. Problem

aish has no first-class error type. Each of its three failure surfaces is
handled ad hoc, and none of them tells the user *where* the problem is or *how*
to fix it:

| Surface | Today | Symptom |
|---|---|---|
| **Parse / routing** | `tokenize*` return `Option::None`; the line silently routes to the model | A mistyped quote is indistinguishable from genuine prose intent — the user never learns the line *looked like* a command that failed to parse. |
| **Config (`~/.aishrc`)** | a bad line is dropped with a dim `eprintln!`, no location, no code | `export A=1 B=2` is silently skipped at startup; the user gets no line number and no "expected `export NAME=value`" hint. |
| **Exec** | spawn / resolve failures are flat strings (`aish: foo: command not found`) | No did-you-mean, no code, nothing structured to grep for. |

The through-line: aish *knows* something went wrong but throws away the
location, a stable identifier, and the fix. For an AI-native shell whose whole
value proposition is clarity over a hidden `bash`, that silence is a UX gap.

### Why this matters now

S7 is the "diagnostics & legibility" arc. The first brick is giving aish a
single, real diagnostic type so every later surface (plugins, scripts, richer
routing explanations) renders through one consistent channel instead of
sprinkling more `eprintln!`s.

---

## 2. Goal

Adopt **miette** — the MIT-licensed diagnostic engine behind nushell's error
reports — to give aish its own diagnostic surface:

- **byte-span carets** that point at the offending character,
- **stable `aish::…` codes** that are greppable and documentable,
- **`help:` hints** that say how to fix it,

across **parse / config / exec** errors — **without changing routing
semantics**. miette is adopted as a dependency (not reimplemented clean-room):
it already solves span rendering, theming, and `NO_COLOR` correctly, and
matching its output by hand would be wasted effort.

### The one non-negotiable invariant

> The silent route-to-model fallback stays **byte-for-byte** unchanged.

Diagnostics are an *additive* surface. They render only when the user has
**already declared intent to run a shell command** — i.e.:

1. a **forced-shell** line (`!…`),
2. a line in **`~/.aishrc`** (which is unambiguously config, not prose),
3. an **exec** failure (the command resolved to a real attempt to spawn).

A normal auto-routed prose line that happens not to tokenize still flows to the
model with **no output** — exactly as today. This is the explicit non-goal that
guards against turning every "what's eating my disk?" into an error spew.

---

## 3. Acceptance criterion (card)

> **a routing/parse error renders with a caret + code + hint**

Concretely: `!what's eating my disk` (an unbalanced `'`) at a forced-shell
prompt renders a caret under the apostrophe, the code
`aish::parse::unbalanced_quote`, and a `help:` line — in both the color and
`NO_COLOR` themes.

---

## 4. Design

### 4.1 One diagnostic type

A new module `src/diag.rs` defines a single `AishDiagnostic` enum that derives
`miette::Diagnostic` (on top of `thiserror::Error`). Each variant carries:

- `#[source_code]` — the offending source string (the command line, or one
  `~/.aishrc` line), owned via `NamedSource<String>` so the diagnostic can
  outlive the borrowed input,
- `#[label]` — a `SourceSpan` (byte offset + length) that places the caret,
- `#[diagnostic(code(...))]` — the stable code,
- `#[help]` — the fix hint.

### 4.2 The six stable codes

The codes are a **public contract** — greppable, documentable, and not renamed
without a CHANGELOG note:

| Code | Surface | Fires when |
|---|---|---|
| `aish::parse::unbalanced_quote` | parse | a `'` or `"` is never closed |
| `aish::parse::unsupported_meta` | parse | a metachar / backtick aish doesn't run directly appears |
| `aish::parse::empty_stage` | parse | a pipeline has an empty stage (`a \| \| b`) |
| `aish::parse::bad_var_ref` | parse | a `$…` reference is malformed |
| `aish::config::bad_export` | config | a `~/.aishrc` line isn't a valid `export NAME=value` |
| `aish::exec::not_found` | exec | a resolved command can't be found on `$PATH` |

`empty_stage` ships defined + unit-tested even though its only v1 *producer* is
deferred (the pipeline tokenizer stays `Option`-based for now) — keeping the
contract complete so a later pipeline refactor can adopt it without a new code.

### 4.3 Span-aware tokenizer, zero behavior change

The core refactor is a span-aware sibling of the existing tokenizer:
`rc::tokenize_diagnosed` returns `Result<Vec<String>, AishDiagnostic>` instead
of `Option`. It is the *same lexing rules* as `tokenize_with`, but it iterates
`char_indices()` to track byte offsets and every existing `return None` becomes
a typed, spanned `Err`.

The existing `tokenize` / `tokenize_with` / `tokenize_pipeline` become thin
`.ok()` shims over the new core. That is the mechanism that makes the invariant
in §2 literally true: the silent path calls the same code and discards the
diagnostic, so its behavior cannot drift. An AC-level property test pins
`tokenize(x) == tokenize_diagnosed(x, …).ok()` over a corpus.

### 4.4 Located config diagnostics

The three `eprintln!` skip-sites in `rc::parse_into` become coded, located
`BadConfigLine` diagnostics with a `~/.aishrc:N` header (line number from
`.lines().enumerate()`). Crucially, **parsing still continues past a bad line** —
a single malformed export does not abort the rest of the rc file. The span is
relative to the offending line, so the caret stays tight.

### 4.5 Exec did-you-mean

The `command not found` site renders `ExecFailed` with an optional `$PATH`-aware
hint: a **cheap, bounded** Levenshtein (≤ 2, length-gated) over PATH basenames,
reusing the existing PATH enumeration. This is deliberately *not* a full
spell-checker — `None` when nothing is close. `ExecFailed` carries no source
span (there's no line to caret); it renders code + message + optional help.

### 4.6 One theme switch, honoring NO_COLOR

A single `diag::render` chooses miette's `GraphicalTheme::unicode()` vs
`GraphicalTheme::none()` from the existing `style::colors_enabled()` — the same
switch that already honors `--no-color`, `NO_COLOR`, and non-TTY output. The
`none()` theme still emits a caret, the code, and the `help:` line in plain
ASCII: **color changes the glyphs, never the information**.

---

## 5. Acceptance criteria (testable)

1. A forced-shell parse failure renders a caret + an `aish::parse::…` code + a
   `help:` line (asserted against the plain theme, so it's TTY-independent).
2. `tokenize` / `tokenize_with` / `tokenize_pipeline` are byte-for-byte
   unchanged; the existing tokenizer test suite passes untouched.
3. Each of the six codes is stable and unit-tested (present in render output).
4. Span offsets equal the offending char index — the `|` in `a | | b`, the
   opening `'` in an unbalanced quote, the `B` in `export A=1 B=2` — including a
   multibyte case (byte, not char, offset).
5. A malformed `~/.aishrc` line yields a coded/located diagnostic **and** rc
   parsing continues for the good lines.
6. `NO_COLOR` → no ANSI escape but still caret + code + help; color on →
   graphical theme.

---

## 6. Non-goals

- **No routing-semantics change.** The default REPL fallback stays silent; this
  is the load-bearing invariant, not a nice-to-have.
- **No `$PATH`-wide spell-checker** — the exec hint is bounded edit-distance
  only, infrastructure for a richer suggester later.
- **No i18n / narratable localization.**
- **No wholesale `anyhow` → `miette` migration** — `AishDiagnostic` is purely
  additive; existing `anyhow` error paths are untouched.
- **No spanned pipeline tokenizer in v1** — `empty_stage` is defined and
  unit-tested, but its producer is deferred.

---

## 7. Dependencies & licensing

Two crates: `miette` (with the `fancy` feature for the graphical renderer) and
`thiserror`. Both are MIT / Apache-2.0-family and compatible with aish's
`MIT OR Apache-2.0`. The exact SPDX strings are confirmed from `cargo metadata`
and recorded in `THIRD_PARTY_NOTICES.md` before the dependency lands — if either
turns out copyleft, that step stops and escalates. The `fancy` feature must not
break `cargo build --no-default-features` (it's independent of aish's `local`
feature).

---

## 8. Rollout

Bisectable, PR-sized commits, each compiling and testing green on its own:
dependency + notices → `src/diag.rs` → `tokenize_diagnosed` + shims → config
conversion → exec mapping → forced-shell surfacing → CHANGELOG. See the
engineering spec §7 for the exact sequence and §6 for the test matrix.

---

## 9. Future work

- A spanned pipeline tokenizer that *produces* `empty_stage`.
- A richer exec suggester (frequency-weighted, alias-aware).
- Routing the model-fallback path's *own* "why did this route to the model"
  explanation through the same diagnostic channel (opt-in, verbose mode).
- Plugin and script load errors adopting `AishDiagnostic` codes.
