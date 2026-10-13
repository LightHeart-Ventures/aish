## What changed

<!-- One or two lines. What does this PR do, and why now? -->

## Evidence

<!-- REQUIRED. Paste the actual command + result. "Tests pass" is not evidence. -->

```
$ cargo test --no-default-features --locked
...
```

- [ ] `cargo test --no-default-features --locked` — green
- [ ] `cargo clippy --all-targets --no-default-features --locked -- -D warnings` — clean
- [ ] Touched the voice feature? `cargo build --features voice` — green
- [ ] Touched `src/repl.rs` / the TUI? REPL smoke considered

## Risk

<!-- Blast radius if this is wrong, and how to back it out. -->

- Reversible by: <!-- revert commit / config flip / N/A -->
- Affected surfaces: <!-- e.g. REPL input path, worker dispatch, broker protocol -->

## Linked work

<!-- Atum TASK-### / ISS-###, or the originating request. -->
