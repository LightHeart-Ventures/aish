# Error Diagnostics — miette-backed spans, codes, and hints

**Card:** TASK-139 (`card_8aa65edb8b5a`) · sprint `sprint_ceae1c2263d6` · 5 pts · high

This document covers both the design rationale (§1–6 below) and the engineering specification (§7–10 below). For the companion turn-key runbook of exact signatures and test sequencing, see the sections marked "Implementation".

---

## §1. Problem

aish has no first-class error type. Each of its three failure surfaces is handled ad hoc, and none of them tells the user *where* the problem is or *how* to fix it:

| Surface | Today | Symptom |
|---|---|---|
| **Parse / routing** | `tokenize*` return `Option::None`; the line silently routes to the model | A mistyped quote is indistinguishable from genuine prose intent — the user never learns the line *looked like* a command that failed to parse. |
| **Config (`~/.aishrc`)** | a bad line is dropped with a dim `eprintln!`, no location, no code | `export A=1 B=2` is silently skipped at startup; the user gets no line number and no "expected `export NAME=value`" hint. |
| **Exec** | spawn / resolve failures are flat strings (`aish: foo: command not found`) | No did-you-mean, no code, nothing structured to grep for. |

The through-line: aish *knows* something went wrong but throws away the location, a stable identifier, and the fix. For an AI-native shell whose whole value proposition is clarity over a hidden `bash`, that silence is a UX gap.

### Why this matters now

S7 is the "diagnostics & legibility" arc. The first brick is giving aish a single, real diagnostic type so every later surface (plugins, scripts, richer routing explanations) renders through one consistent channel instead of sprinkling more `eprintln!`s.

---

## §2. Design goal

Adopt **miette** — the MIT-licensed diagnostic engine behind nushell's error reports — to give aish its own diagnostic surface:

- **byte-span carets** that point at the offending character,
- **stable `aish::…` codes** that are greppable and documentable,
- **`help:` hints** that say how to fix it,

across **parse / config / exec** errors — **without changing routing semantics**. miette is adopted as a dependency (not reimplemented clean-room): it already solves span rendering, theming, and `NO_COLOR` correctly, and matching its output by hand would be wasted effort.

### The one non-negotiable invariant

> The silent route-to-model fallback stays **byte-for-byte** unchanged.

Diagnostics are an *additive* surface. They render only when the user has **already declared intent to run a shell command** — i.e.:

1. a **forced-shell** line (`!…`),
2. a line in **`~/.aishrc`** (which is unambiguously config, not prose),
3. an **exec** failure (the command resolved to a real attempt to spawn).

A normal auto-routed prose line that happens not to tokenize still flows to the model with **no output** — exactly as today. This is the explicit non-goal that guards against turning every "what's eating my disk?" into an error spew.

---

## §3. Acceptance criterion (card)

> **a routing/parse error renders with a caret + code + hint**

Concretely: `!what's eating my disk` (an unbalanced `'`) at a forced-shell prompt renders a caret under the apostrophe, the code `aish::parse::unbalanced_quote`, and a `help:` line — in both the color and `NO_COLOR` themes.

---

## §4. Design highlights

### 4.1 One diagnostic type

A new module `src/diag.rs` defines a single `AishDiagnostic` enum that derives `miette::Diagnostic` (on top of `thiserror::Error`). Each variant carries:

- `#[source_code]` — the offending source string (the command line, or one `~/.aishrc` line), owned via `NamedSource<String>` so the diagnostic can outlive the borrowed input,
- `#[label]` — a `SourceSpan` (byte offset + length) that places the caret,
- `#[diagnostic(code(...))]` — the stable code,
- `#[help]` — the fix hint.

### 4.2 The six stable codes

The codes are a **public contract** — greppable, documentable, and not renamed without a CHANGELOG note:

| Code | Surface | Fires when |
|---|---|---|
| `aish::parse::unbalanced_quote` | parse | a `'` or `"` is never closed |
| `aish::parse::unsupported_meta` | parse | a metachar / backtick aish doesn't run directly appears |
| `aish::parse::empty_stage` | parse | a pipeline has an empty stage (`a \| \| b`) |
| `aish::parse::bad_var_ref` | parse | a `$…` reference is malformed |
| `aish::config::bad_export` | config | a `~/.aishrc` line isn't a valid `export NAME=value` |
| `aish::exec::not_found` | exec | a resolved command can't be found on `$PATH` |

`empty_stage` ships defined + unit-tested even though its only v1 *producer* is deferred (the pipeline tokenizer stays `Option`-based for now) — keeping the contract complete so a later pipeline refactor can adopt it without a new code.

### 4.3 Span-aware tokenizer, zero behavior change

The core refactor is a span-aware sibling of the existing tokenizer: `rc::tokenize_diagnosed` returns `Result<Vec<String>, AishDiagnostic>` instead of `Option`. It is the *same lexing rules* as `tokenize_with`, but it iterates `char_indices()` to track byte offsets and every existing `return None` becomes a typed, spanned `Err`.

The existing `tokenize` / `tokenize_with` / `tokenize_pipeline` become thin `.ok()` shims over the new core. That is the mechanism that makes the invariant in §2 literally true: the silent path calls the same code and discards the diagnostic, so its behavior cannot drift. An AC-level property test pins `tokenize(x) == tokenize_diagnosed(x, …).ok()` over a corpus.

### 4.4 Located config diagnostics

The three `eprintln!` skip-sites in `rc::parse_into` become coded, located `BadConfigLine` diagnostics with a `~/.aishrc:N` header (line number from `.lines().enumerate()`). Crucially, **parsing still continues past a bad line** — a single malformed export does not abort the rest of the rc file. The span is relative to the offending line, so the caret stays tight.

### 4.5 Exec did-you-mean

The `command not found` site renders `ExecFailed` with an optional `$PATH`-aware hint: a **cheap, bounded** Levenshtein (≤ 2, length-gated) over PATH basenames, reusing the existing PATH enumeration. This is deliberately *not* a full spell-checker — `None` when nothing is close. `ExecFailed` carries no source span (there's no line to caret); it renders code + message + optional help.

### 4.6 One theme switch, honoring NO_COLOR

A single `diag::render` chooses miette's `GraphicalTheme::unicode()` vs `GraphicalTheme::none()` from the existing `style::colors_enabled()` — the same switch that already honors `--no-color`, `NO_COLOR`, and non-TTY output. The `none()` theme still emits a caret, the code, and the `help:` line in plain ASCII: **color changes the glyphs, never the information**.

---

## §5. Non-goals

- **No routing-semantics change.** The default REPL fallback stays silent; this is the load-bearing invariant, not a nice-to-have.
- **No `$PATH`-wide spell-checker** — the exec hint is bounded edit-distance only, infrastructure for a richer suggester later.
- **No i18n / narratable localization.**
- **No wholesale `anyhow` → `miette` migration** — `AishDiagnostic` is purely additive; existing `anyhow` error paths are untouched.
- **No spanned pipeline tokenizer in v1** — `empty_stage` is defined and unit-tested, but its producer is deferred.

---

## §6. Dependencies & licensing

Add to `Cargo.toml [dependencies]`:

```toml
miette = { version = "7", features = ["fancy"] }
thiserror = "2"
```

- `miette` provides the `Diagnostic` derive, `SourceSpan`, `NamedSource`, and the `fancy` graphical/narratable report renderers.
- `thiserror` provides `#[derive(Error)]` so `AishDiagnostic` is a real `std::error::Error` (miette's `Diagnostic` derives on top of it).
- **`fancy`** pulls `owo-colors` + `supports-color` + `unicode-width`. aish already depends on `unicode-width`; no new heavy transitive trees.

**Licensing gate (blocks the dep landing):** verify `miette` and `thiserror` licenses with `cargo metadata` before committing. Both are expected Apache-2.0 / MIT-family and compatible with aish's `MIT OR Apache-2.0`. Add a row for each to `THIRD_PARTY_NOTICES.md` → *Direct dependencies* table. (Confirm the SPDX string from `cargo metadata` and record the real one.) If either is copyleft, STOP and escalate.

Acceptance for this step: `cargo build` and `cargo build --no-default-features` both succeed (the `local` feature must not interact), and `THIRD_PARTY_NOTICES.md` lists both crates.

---

## §7 (Implementation). New module: `src/diag.rs`

Wire into `src/main.rs` module list (`mod diag;`).

### 7.1 The diagnostic enum

```rust
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum AishDiagnostic {
    // ── parse / routing (src is the offending command line) ──────────────
    #[error("unbalanced quote")]
    #[diagnostic(
        code(aish::parse::unbalanced_quote),
        help("close the quote, or escape it; aish routes unparseable lines to the model")
    )]
    UnbalancedQuote {
        #[source_code] src: NamedSource<String>,
        #[label("opened here")] span: SourceSpan,
    },

    #[error("unsupported shell syntax")]
    #[diagnostic(
        code(aish::parse::unsupported_meta),
        help("aish has no shell underneath — pipes/redirection/globs/substitution aren't run directly")
    )]
    UnsupportedMeta {
        #[source_code] src: NamedSource<String>,
        #[label("not supported here")] span: SourceSpan,
    },

    #[error("empty pipeline stage")]
    #[diagnostic(code(aish::parse::empty_stage), help("every `|` needs a command on both sides"))]
    EmptyStage {
        #[source_code] src: NamedSource<String>,
        #[label("nothing to run")] span: SourceSpan,
    },

    #[error("malformed variable reference")]
    #[diagnostic(code(aish::parse::bad_var_ref), help("use $NAME or ${NAME}"))]
    BadVarRef {
        #[source_code] src: NamedSource<String>,
        #[label("here")] span: SourceSpan,
    },

    // ── config (~/.aishrc:LINE) ──────────────────────────────────────────
    #[error("bad export line")]
    #[diagnostic(code(aish::config::bad_export), help("expected `export NAME=value`"))]
    BadConfigLine {
        #[source_code] src: NamedSource<String>,
        #[label("{reason}")] span: SourceSpan,
        reason: String,
    },

    // ── exec ─────────────────────────────────────────────────────────────
    #[error("command not found: {cmd}")]
    #[diagnostic(code(aish::exec::not_found))]
    ExecFailed {
        cmd: String,
        #[help] hint: Option<String>, // $PATH-aware did-you-mean
    },
}
```

Notes:
- `NamedSource<String>` owns the source string so a diagnostic can outlive the borrowed line (rc lines, in particular). The "name" is the header miette prints: `"<command line>"` for parse errors, `"~/.aishrc:42"` for config.
- The six **stable codes** are the public contract (tested in §10.3). Do not rename without a CHANGELOG note.
- `ExecFailed` carries no `#[source_code]` — there's no line to caret; it renders code + message + optional help only.

### 7.2 The renderer

```rust
/// Render a diagnostic to a String, picking miette's theme from the existing
/// color decision. Caret + code + help appear in BOTH themes — color only
/// changes the glyphs/ANSI, never the information.
pub fn render(d: &AishDiagnostic) -> String {
    let handler = if crate::style::colors_enabled() {
        miette::GraphicalReportHandler::new()
            .with_theme(miette::GraphicalTheme::unicode())
    } else {
        miette::GraphicalReportHandler::new()
            .with_theme(miette::GraphicalTheme::none()) // ASCII, no ANSI
    };
    let mut out = String::new();
    let _ = handler.render_report(&mut out, d);
    out
}

/// Convenience: render to stderr.
pub fn eprint(d: &AishDiagnostic) {
    eprintln!("{}", render(d));
}
```

- Theme is sourced from `crate::style::colors_enabled()` (`src/style.rs:32`), the single switch honoring `--no-color`, `NO_COLOR`, and non-TTY stdout. `GraphicalTheme::none()` emits a caret line, the `aish::…` code, and the `help:` line in plain ASCII — satisfying AC #6 (NO_COLOR still caret+code+help).
- Build the handler per-call; it is cheap and avoids global state. Optionally `OnceLock` later — not needed for v1.

---

## §8 (Implementation). `rc.rs` refactor — span-aware tokenizer

### 8.1 New spanned core

Introduce a span-aware sibling of `tokenize_with` that returns a `Result` instead of an `Option`. The existing `tokenize` / `tokenize_with` / `tokenize_pipeline` become thin `.ok()` shims so **the silent route-to-model path is byte-for-byte unchanged**.

```rust
/// Span-aware tokenizer. Same lexing rules as `tokenize_with`, but every
/// `return None` becomes a spanned diagnostic carrying the byte offset of the
/// offending character. `src_name` is the NamedSource header (the command line,
/// or "~/.aishrc:N").
pub fn tokenize_diagnosed(
    line: &str,
    src_name: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, AishDiagnostic> { … }
```

Implementation = the current `tokenize_with` body, tracking the **byte offset** of each char (iterate `line.char_indices()` instead of `chars()`), and replacing each failure with a typed error:

| Current `return None` | Diagnostic | Span (byte offset) |
|---|---|---|
| `'\''` loop hits EOF | `UnbalancedQuote` | index of the opening `'` |
| `'"'` loop hits EOF | `UnbalancedQuote` | index of the opening `"` |
| `'"'` loop hits `` ` `` | `UnsupportedMeta` | index of the `` ` `` |
| `META.contains(&c)` | `UnsupportedMeta` | index of the metachar |
| `expand_dollar` → `None` | `BadVarRef` | index of the `$` |

`expand_dollar` must surface *where* it failed. Cheapest path: have `tokenize_diagnosed` record the byte offset of the `$` before calling, and on `None` synthesize `BadVarRef { span: dollar_at.into() }`. (No need to thread spans through `expand_dollar` itself for v1 — the `$` position is sufficient and matches AC granularity.)

### 8.2 Back-compat shims (zero behavior change)

```rust
pub fn tokenize(line: &str) -> Option<Vec<String>> {
    tokenize_with(line, |n| std::env::var(n).ok())
}
pub fn tokenize_with(line: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    tokenize_diagnosed(line, "<command line>", lookup).ok()
}
```

`tokenize_pipeline` (`rc.rs:415`) and `pipeline::parse` (`src/pipeline.rs:24`) keep returning `Option`. A spanned pipeline variant is **out of scope** for v1; `EmptyStage` is exercised via a forced-shell surfacing only if a spanned pipeline path is added — otherwise `EmptyStage`/`empty_stage` is still defined and unit-tested directly (construct + render), satisfying §10.3 without a producer. (Document this: the code exists and is stable even though the only v1 producer is the unit test. Keeps the contract complete and lets a later pipeline refactor adopt it.)

> Risk guard: the existing `rc.rs` test suite (`tokenizer_*`, `pipeline_*`) must pass **untouched** — that is AC #2. Run `cargo test -p aish rc::` after the refactor and diff nothing.

### 8.3 Config skip-site conversion (`parse_into`, rc.rs)

Convert the three `eprintln!` skip sites in `parse_into` to render coded/located diagnostics, **while still `continue`-ing past the bad line** (AC #5 — parsing must not abort):

1. command-substitution `` ` `` in an export value → `BadConfigLine { reason: "command substitution needs a shell", span: <index of `` ` ``> }`
2. `split_assignment` → `None` (not a plain `NAME=value`) → `BadConfigLine { reason: "not a plain NAME=value export", span: <index of first offending char, e.g. the space in `A=1 B=2`> }`
3. (optional) a malformed alias value that fails to tokenize → reuse `BadConfigLine` or skip silently as today; v1 may leave alias handling unchanged.

`parse_into` needs the **line number** for the `~/.aishrc:N` header. Today it iterates `text.lines()`; add `.enumerate()` and pass `format!("{src_name}:{}", n + 1)` as the NamedSource name. Thread an optional `src_name: &str` parameter (default `"~/.aishrc"` from `load`; profile files pass their own path from `load_login_profiles`).

Span offsets in config are **relative to the offending line**, with the NamedSource string being that single line (not the whole file) — keeps offsets small and the caret correct.

### 8.4 Exec NotFound mapping (`repl.rs::dispatch`)

At the `resolve_program` → `None` site (`src/repl.rs`, the `cmd =>` arm, ~`let Some(path) = resolve_program(...) else`), replace:

```rust
eprintln!("aish: {cmd}: command not found");
```

with a rendered `ExecFailed`:

```rust
let hint = diag::path_suggestion(cmd, &path_var); // Option<String>, cheap
crate::diag::eprint(&AishDiagnostic::ExecFailed { cmd: cmd.into(), hint });
```

`path_suggestion` is a **cheap** did-you-mean: scan PATH basenames (reuse the same enumeration as `repl::scan_path_commands`) and return the closest by a bounded edit-distance (Levenshtein ≤ 2, length-gated). **Non-goal:** no full PATH spell-checker, no fuzzy index — `None` when nothing is within distance. Keep it O(PATH entries) and skip when `cmd` is long/odd.

Surface this only on the forced (`!`) and auto paths that currently print `command not found`; the silent route-to-model path (`Dispatch::NotACommand`) is untouched.

---

## §9 (Implementation). Forced-shell surfacing (the AC producer)

The card AC — *a routing/parse error renders with a caret + code + hint* — is satisfied at the **forced-direct** site in `dispatch` (`src/repl.rs`, the `rc::tokenize_with(...) else` block, ~line 1621) and its twin in `src/script.rs` (~line 202).

Today, when a `!`-forced line fails to tokenize:

```rust
let Some(mut words) = rc::tokenize_with(line, var_lookup(session)) else {
    if force {
        eprintln!("aish: can't run that directly — it uses shell syntax aish doesn't implement");
        return Dispatch::Handled;
    }
    return Dispatch::NotACommand;   // ← silent route-to-model: UNCHANGED
};
```

Change the `force` branch to call the spanned tokenizer and render:

```rust
let words = match rc::tokenize_diagnosed(line, "<command line>", var_lookup(session)) {
    Ok(w) => w,
    Err(d) if force => { crate::diag::eprint(&d); return Dispatch::Handled; }
    Err(_) => return Dispatch::NotACommand, // unchanged silent fallback
};
```

Key invariant: **only `force` (or rc/exec contexts) ever renders a diagnostic.** A normal auto-routed prose line still hits `Err(_) => NotACommand` and flows to the model with no output — preserving routing semantics (the explicit non-goal).

This gives the AC its end-to-end demo: `!what's eating my disk` → caret under the apostrophe + `aish::parse::unbalanced_quote` + `help:` line.

---

## §10 (Implementation). Test plan (maps to design acceptance criteria)

All tests are `#[cfg(test)]` unit tests co-located in `src/diag.rs` and `src/rc.rs`, plus one integration test. Run with `cargo test` (and once with `cargo build --no-default-features` to confirm the dep is feature-independent).

| # | AC | Test | Location |
|---|---|---|---|
| 1 | forced parse failure renders caret + `aish::parse::` code + `help:` | render `UnbalancedQuote` with `GraphicalTheme::none()`, assert output contains `^`/caret, `aish::parse::unbalanced_quote`, `help:` | `diag.rs` |
| 2 | `tokenize`/`tokenize_pipeline` byte-for-byte unchanged | existing `rc.rs` tests pass untouched; add a property check that `tokenize(x) == tokenize_diagnosed(x,…).ok()` over the corpus | `rc.rs` |
| 3 | each of the six codes stable | one `assert!(render(&d).contains("aish::…::…"))` per variant | `diag.rs` |
| 4 | span offset == offending char index | `a \| \| b` → `\|` index; unbalanced `'` → `'` index; `export A=1 B=2` → `B` index. Assert `SourceSpan::offset()` | `rc.rs` |
| 5 | malformed `~/.aishrc` line → coded/located diagnostic AND parsing continues | parse a 3-line rc with a bad middle line; assert the good lines still land in `Rc` AND a `BadConfigLine` was produced (capture via a collector param or a `parse_into_diagnosed` returning `Vec<AishDiagnostic>`) | `rc.rs` |
| 6 | NO_COLOR → no ANSI but caret+code+help; color on → graphical | render same diagnostic under both themes; assert `none()` output has no `\x1b[`, both contain caret+code+help | `diag.rs` |

**Test seam for AC #5:** `parse_into` currently returns `()`. To assert a diagnostic was emitted without scraping stderr, add an internal `parse_into_diagnosed(text, rc, src_name) -> Vec<AishDiagnostic>` that `parse_into` wraps (rendering each to stderr). Tests call the `_diagnosed` form and inspect the `Vec`. Keeps production behavior (render to stderr, continue) while making emission observable.

**Integration test (optional, recommended):** add to `tests/` a forced-shell case asserting the rendered string shape, mirroring the `golden_routing_heuristics` style. Not a PTY test — `render()` is pure, so a plain string assertion suffices.

---

## §11 (Implementation). Sequencing (PR-sized commits)

1. **dep + notices** — `Cargo.toml` add, `THIRD_PARTY_NOTICES.md` rows, license verify. `cargo build` ×2. *(blocks on §6 license gate)*
2. **`src/diag.rs`** — enum + `render`/`eprint` + §10.1/10.3/10.6 unit tests. Self-contained; no callers yet.
3. **`rc::tokenize_diagnosed`** + `.ok()` shims + §10.2/10.4 tests. No behavior change.
4. **config conversion** — `parse_into` → `_diagnosed` seam + §10.5 test.
5. **exec mapping** — `ExecFailed` + `path_suggestion` at the `resolve_program` None site.
6. **forced-shell surfacing** — `repl.rs` + `script.rs` `force` branches; AC end-to-end.
7. **CHANGELOG.md** entry under Unreleased; final `cargo test` + `cargo clippy`.

Each step compiles and tests green on its own, so the branch is bisectable.

---

## §12 (Implementation). Risks & mitigations

| Risk | Mitigation |
|---|---|
| Behavior drift in the silent route-to-model path | Shims are literal `.ok()` of the new core; AC #2 corpus + property test pin equality |
| Byte vs char offset bugs (multibyte input) | Use `char_indices()` (byte offsets); `SourceSpan` is byte-based, matching `NamedSource`. Add a multibyte case to §10.4 |
| `fancy` feature bloats the no-default build | Verify `cargo build --no-default-features`; miette is independent of the `local` feature |
| License surprise | §6 gate blocks the dep until SPDX confirmed and recorded |
| Over-eager diagnostics changing UX | Only `force` / rc / exec sites render; auto-route stays silent (enforced by the `Err(_) => NotACommand` arm) |

---

## §13 (Implementation). Definition of done

- [ ] `miette` + `thiserror` added, licenses verified, `THIRD_PARTY_NOTICES.md` updated.
- [ ] `src/diag.rs` with `AishDiagnostic` (6 codes) + `render`/`eprint`.
- [ ] `rc::tokenize_diagnosed` lands; `tokenize`/`tokenize_with`/`tokenize_pipeline` unchanged via shims.
- [ ] `parse_into` emits located `BadConfigLine`, parsing continues.
- [ ] exec `command not found` renders `ExecFailed` with optional `$PATH` hint.
- [ ] forced-shell parse failure renders caret + code + help (card AC).
- [ ] All six design acceptance criteria covered by tests; `cargo test` + `cargo test --no-default-features` + `cargo clippy` green.
- [ ] CHANGELOG.md entry.
