# Agent guardrails for aish

Instructions for any AI agent (aish coordinator, Claude Code, etc.) working in
this repo. The `.repospec.json` is the machine-readable source of truth; this
file restates the load-bearing rules in prose.

## Build & test validation: ALWAYS use Blacksmith Testbox — never local cargo

Local `cargo build`/`cargo test` of aish **OOM-kills** in coordinator worktrees
and on low-memory hosts because the default `local` feature pulls in the heavy
llama.cpp / mistralrs native graph (see `aish_sre` skill §3). Run every build,
test, and pre-commit validation on **Blacksmith Testbox** (https://blacksmith.sh)
instead — it runs the exact CI gate remotely on a warm 4-vcpu VM.

**Procedure** (full details in the `blacksmith-testbox` skill):

```sh
# 1. Warm up once — returns an ID instantly, VM boots in background:
blacksmith testbox warmup .github/workflows/ci-testbox.yml --job testbox
#   → tbx_...  (save the ID)

# 2. Run the exact CI gate against the warm VM (auto-waits if still booting):
blacksmith testbox run --id <ID> "cargo test --no-default-features --locked"

# 3. Stop when done to free resources:
blacksmith testbox stop --id <ID>
```

The CI gate is **`cargo test --no-default-features --locked`** — it must match
`.github/workflows/ci.yml` and the warmup in `.github/workflows/ci-testbox.yml`.
Do **not** add `--features local` to test runs; that flag is reserved for the
release build only.

**The only exception:** trivial, zero-dependency, non-compiling checks with no
OOM risk — `cargo fmt --check`, `cargo clippy` on a tiny surface. Everything
that compiles the crate or runs tests goes through Blacksmith.

## Other standing rules

- Feature branches + draft PRs only. Never push to `main`; never push a release
  tag without org-admin bypass. (See `aish_sre` skill.)
- Version bumps: edit **both** `Cargo.toml` and `Cargo.lock` (CI runs `--locked`).
- Read the `aish_sre` skill before cutting a release or debugging CI/build failures.
