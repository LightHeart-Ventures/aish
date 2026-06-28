# Engineering Spec — TASK-139 / S7.1: miette-backed diagnostics (spans + codes + hints)

**Card:** TASK-139 (`card_8aa65edb8b5a`) · sprint `sprint_ceae1c2263d6` · 5 pts · high
**PRD:** card `productSpec` + `docs/S7.1-miette-diagnostics.md`
**AC (card):** *a routing/parse error renders with a caret + code + hint*
**Status target:** Analyze → Develop

---

## 1. Summary

aish has no first-class error type. Three failure surfaces are each handled ad hoc:

| Surface | Today | File / site |
|---|---|---|
| Parse / routing | `tokenize*` return `Option::None`; the line silently routes to the model | `src/rc.rs` (`tokenize_with`, `expand_dollar`, `split_pipeline`) |
| Config (`~/.aishrc`) | bad lines dropped with a dim `eprintln!`, no location/code | `src/rc.rs::parse_into` (3 skip sites) |
| Exec | spawn/resolve failures are flat strings | `src/repl.rs::dispatch` (`command not found`), `tools::run_on_tty` |

This work adopts **miette** to give aish a single diagnostic surface — byte-span carets, stable `aish::…` codes, and `help:` hints — **without changing routing semantics**. The silent route-to-model fallback stays exactly as-is; diagnostics are an *additive* surface used (a) when the user forces a shell parse (`!`), (b) on a bad rc line, and (c) on an exec failure.

This document is the build plan: module layout, exact signatures, integration points, sequencing, and the test matrix. The design rationale lives in the PRD; this spec assumes it.

---

## 2. Dependencies & licensing

Add to `Cargo.toml [dependencies]`:

```toml
miette = { version = "7", features = ["fancy"] }
thiserror = "2"
```

- `miette` provides the `Diagnostic` derive, `SourceSpan`, `NamedSource`, and the `fancy` graphical/narratable report renderers.
- `thiserror` provides `#[derive(Error)]` so `AishDiagnostic` is a real `std::error::Error` (miette's `Diagnostic` derives on top of it).
- **`fancy`** pulls `owo-colors` + `supports-color` + `unicode-width`. aish already depends on `unicode-width`; no new heavy transitive trees.

**Licensing gate (blocks the dep landing):** verify `miette` and `thiserror` licenses with `cargo metadata` before committing. Both are expected Apache-2.0 / MIT-family and compatible with aish's `MIT OR Apache-2.0`. Add a row for each to `THIRD_PARTY_NOTICES.md` → *Direct dependencies* table. (The PRD's "MIT" note is approximate — confirm the SPDX string from `cargo metadata` and record the real one.) If either is copyleft, STOP and escalate.

Acceptance for this step: `cargo build` and `cargo build --no-default-features` both succeed (the `local` feature must not interact), and `THIRD_PARTY_NOTICES.md` lists both crates.

---

## 3. New module: `src/diag.rs`

Wire into `src/main.rs` module list (`mod diag;`).

### 3.1 The diagnostic enum

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
- The six **stable codes** are the public contract (tested in §6.3). Do not rename without a CHANGELOG note.
- `ExecFailed` carries no `#[source_code]` — there's no line to caret; it renders code + message + optional help only.

### 3.2 The renderer

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

## 4. `rc.rs` refactor — span-aware tokenizer

### 4.1 New spanned core

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

### 4.2 Back-compat shims (zero behavior change)

```rust
pub fn tokenize(line: &str) -> Option<Vec<String>> {
    tokenize_with(line, |n| std::env::var(n).ok())
}
pub fn tokenize_with(line: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    tokenize_diagnosed(line, "<command line>", lookup).ok()
}
```

`tokenize_pipeline` (`rc.rs:415`) and `pipeline::parse` (`src/pipeline.rs:24`) keep returning `Option`. A spanned pipeline variant is **out of scope** for v1; `EmptyStage` is exercised via a forced-shell surfacing in §5 only if a spanned pipeline path is added — otherwise `EmptyStage`/`empty_stage` is still defined and unit-tested directly (construct + render), satisfying §6.3 without a producer. (Document this: the code exists and is stable even though the only v1 producer is the unit test. Keeps the contract complete and lets a later pipeline refactor adopt it.)

> Risk guard: the existing `rc.rs` test suite (`tokenizer_*`, `pipeline_*`) must pass **untouched** — that is AC #2. Run `cargo test -p aish rc::` after the refactor and diff nothing.

### 4.3 Config skip-site conversion (`parse_into`, rc.rs)

Convert the three `eprintln!` skip sites in `parse_into` to render coded/located diagnostics, **while still `continue`-ing past the bad line** (AC #5 — parsing must not abort):

1. command-substitution `` ` `` in an export value → `BadConfigLine { reason: "command substitution needs a shell", span: <index of `` ` ``> }`
2. `split_assignment` → `None` (not a plain `NAME=value`) → `BadConfigLine { reason: "not a plain NAME=value export", span: <index of first offending char, e.g. the space in `A=1 B=2`> }`
3. (optional) a malformed alias value that fails to tokenize → reuse `BadConfigLine` or skip silently as today; v1 may leave alias handling unchanged.

`parse_into` needs the **line number** for the `~/.aishrc:N` header. Today it iterates `text.lines()`; add `.enumerate()` and pass `format!("{src_name}:{}", n + 1)` as the NamedSource name. Thread an optional `src_name: &str` parameter (default `"~/.aishrc"` from `load`; profile files pass their own path from `load_login_profiles`).

Span offsets in config are **relative to the offending line**, with the NamedSource string being that single line (not the whole file) — keeps offsets small and the caret correct.

### 4.4 Exec NotFound mapping (`repl.rs::dispatch`)

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

## 5. Forced-shell surfacing (the AC producer)

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

## 6. Test plan (maps to PRD §5 acceptance criteria)

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

## 7. Sequencing (PR-sized commits on this branch)

1. **dep + notices** — `Cargo.toml` add, `THIRD_PARTY_NOTICES.md` rows, license verify. `cargo build` ×2. *(blocks on §2 license gate)*
2. **`src/diag.rs`** — enum + `render`/`eprint` + §6.1/6.3/6.6 unit tests. Self-contained; no callers yet.
3. **`rc::tokenize_diagnosed`** + `.ok()` shims + §6.2/6.4 tests. No behavior change.
4. **config conversion** — `parse_into` → `_diagnosed` seam + §6.5 test.
5. **exec mapping** — `ExecFailed` + `path_suggestion` at the `resolve_program` None site.
6. **forced-shell surfacing** — `repl.rs` + `script.rs` `force` branches; AC end-to-end.
7. **CHANGELOG.md** entry under Unreleased; final `cargo test` + `cargo clippy`.

Each step compiles and tests green on its own, so the branch is bisectable.

---

## 8. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Behavior drift in the silent route-to-model path | Shims are literal `.ok()` of the new core; AC #2 corpus + property test pin equality |
| Byte vs char offset bugs (multibyte input) | Use `char_indices()` (byte offsets); `SourceSpan` is byte-based, matching `NamedSource`. Add a multibyte case to §6.4 |
| `fancy` feature bloats the no-default build | Verify `cargo build --no-default-features`; miette is independent of the `local` feature |
| License surprise | §2 gate blocks the dep until SPDX confirmed and recorded |
| Over-eager diagnostics changing UX | Only `force` / rc / exec sites render; auto-route stays silent (enforced by the `Err(_) => NotACommand` arm) |

---

## 9. Out of scope (non-goals, per PRD)

- No routing-semantics change — the default REPL fallback stays silent.
- No `$PATH`-wide spell-checker — `path_suggestion` is bounded edit-distance only.
- No i18n / `narratable` localization.
- No wholesale `anyhow` → `miette` migration — `AishDiagnostic` is additive.
- No spanned pipeline tokenizer in v1 (`empty_stage` code exists + is unit-tested, but its only producer is deferred).

---

## 10. Definition of done

- [ ] `miette` + `thiserror` added, licenses verified, `THIRD_PARTY_NOTICES.md` updated.
- [ ] `src/diag.rs` with `AishDiagnostic` (6 codes) + `render`/`eprint`.
- [ ] `rc::tokenize_diagnosed` lands; `tokenize`/`tokenize_with`/`tokenize_pipeline` unchanged via shims.
- [ ] `parse_into` emits located `BadConfigLine`, parsing continues.
- [ ] exec `command not found` renders `ExecFailed` with optional `$PATH` hint.
- [ ] forced-shell parse failure renders caret + code + help (card AC).
- [ ] All six PRD acceptance criteria covered by tests; `cargo test` + `cargo test --no-default-features` + `cargo clippy` green.
- [ ] CHANGELOG.md entry.
