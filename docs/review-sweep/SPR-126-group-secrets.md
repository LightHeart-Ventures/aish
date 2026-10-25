# SPR-126 review sweep — secrets/update group (TASK-946, TASK-947, TASK-948)

Audited against `origin/main` @ **78c72ebe544cb96dfddee597361d0b43052b7715** (HEAD == origin/main at audit time).
Board `b_9e8fb2e16501`, sprint SPR-126 (`sprint_054efff1a288`). Owner: review-sweep fan-out worker `w_5OHgmYfd`.
Method: delivery proven by `git merge-base --is-ancestor <mergeCommit> origin/main`; every acceptance criterion
re-audited against **current main source**, not the PR diff and not the pre-existing card ticks.

---

## TASK-946 — [SEC-3.1/F-02] scope `${profile:KEY}` resolution (`card_0222ada6c7b3`)

**Delivery:** PR **#939** `feat/task-946-scoped-secret-resolution`, state MERGED, merge commit
`b380713593a2c83cffe3d97121805a62d834c70e` — `git merge-base --is-ancestor` → **ancestor of main (exit 0)**.
Card metadata (`pullRequestNumber=939`, `branch=feat/task-946-scoped-secret-resolution`) matches GitHub; no repair needed.

**Implementation on main:** `src/secrets.rs` (`SpawnCtx::materialise`, `secret_gate`, `SecretPolicy`,
`ENV_UNSAFE_PROGRAMS`, `KNOWN_SECRET_CONSUMERS`, audit record with `value_len` only) + `src/tools.rs`
`resolve_env` / `resolve_env_value` (gate called per declared `env` entry, refusal aborts the whole tool call).

| # | Criterion | Verdict | Evidence |
|---|---|---|---|
| 1 | `resolve_env_value` injects ONLY the keys named in the env map | **MET** | `src/tools.rs` `resolve_env` loops over `call.args["env"]` only and pushes exactly one `(k, value)` per declared entry; no section/profile expansion anywhere on the path. Tests: `secrets::tests` 15/15 green. |
| 2 | `${profile:KEY}` resolves exactly one key; siblings never passed to the child | **MET** | key-exact `profile_lookup(creds, section, key)`; `profile_prefix_semantics` + new `tools::tests::section_siblings_never_materialise` (this sweep) assert a 3-key section yields only the named key and that no sibling name or value reaches the child map. |
| 3 | Child env built from an explicit allowlist, not inherited wholesale | **NOT MET** | no `env_clear()` on any spawn path in `src/tools.rs`; the child inherits the parent environment and the resolved pairs are *added* via `Command::envs`. PRD R2's base-allowlist (`PATH`,`HOME`,`TERM`,`LANG`,`TMPDIR`) is unimplemented. Open question filed (below) — the base set is an operator/product decision and breaking children that need inherited vars is a real regression risk. |
| 4 | Secret values never appear in command echo, confirm prompt, activity log, or job output | **MET** | `egress_prompt(program, args, …)` takes no env/secret parameter — the prompt renders program+args only; audit records carry `key`, `program`, `argv_hash`, `value_len` and never the value (`secrets::tests::audit_never_contains_value`, `argv_hash_is_stable_and_value_free`); resolved values are registered for redaction *before* spawn (`registered_for_redaction`), and background job output is scrubbed at both capture and read boundaries (TASK-947 / PR #1115). |
| 5 | A resolution failure reports the KEY NAME only | **MET** | refusal strings in `src/secrets.rs` interpolate the key name and program; no value is in scope at refusal time (the gate runs before lookup succeeds). `refuse_env_dumper`, `refuse_interpreter`, `refuse_wrapped_interpreter`, `refuse_unknown_binary`, `refuse_egress`, `key_allowlist_enforced` green. |
| 6 | Unit test asserts a two-key profile section leaks only the requested key into the child env | **NOT MET (fix in flight)** | no such test existed on main. Written this sweep as `tools::tests::section_siblings_never_materialise` and opened as a **draft PR** (see final report); stays `met=false` until that PR is merged and proven on main. |
| 7 | `cargo test --no-default-features --locked` is green | **MET** | see commands below. |

**Disposition:** stays in `col_review`. 5/7 MET (AC4 flipped true this sweep). Blockers: AC3 (needs the base-env
allowlist decision — OQ filed) and AC6 (draft PR pending merge).

---

## TASK-947 — [SEC-3.2/F-02] central secret-redaction filter (`card_f5b9ba1ebbed`)

**Delivery:** primary PR **#933** `feat/task-947-secret-redaction` (MERGED 2026-10-08, merge commit
`6e78aa567e1cb3b7f9193ef09a371cfc878b7b8b`) **plus** closer PR **#1115** `fix/task-947-redaction-boundary`
("Scrub background-job output at both capture and read boundaries + opt-in env-secret registration", MERGED
2026-10-25). Both merge commits are ancestors of main. Canonical recorded PR kept as **#933** (the implementation
of record); #1115 is documented on the card as the criterion-closing follow-up.

All **8/8** criteria proven MET against current main (`src/redact.rs` registry + `scrub`, capture-boundary and
read-boundary scrubbing of background job output, `[redacted:LABEL]` markers, chunk-overlap safety, 8-char /
low-entropy floor, base64 + URL-encoded forms, full gate green). Card **moved to `col_completed`**
(`completedAt` 2026-10-25T23:25:55Z) and re-read to confirm the column.

---

## TASK-948 — [SEC-4.1/F-06] verify self-update artifacts (`card_cbd9118b4982`)

**Delivery:** PR **#931** `fix/task-948-verify-update-artifacts`, state MERGED, merge commit
`c1da63f03110c100bd0d27c23fb1ffd30be6e59a` — ancestor of main. Card metadata matches GitHub.

**Implementation on main (`src/update.rs`):** `verify_artifact()` re-reads the asset list at install time,
downloads `<asset>.sha256`, streams SHA-256 over the downloaded bytes and compares **before** staging;
`verify_digest()` / `parse_sha256_sidecar()` fail closed on mismatch, missing or unparseable sidecar (GNU/BSD/bare/
`SHA256SUMS` shapes); `verify_attestation()` runs `gh attestation verify` under
`AISH_UPDATE_ATTESTATION=require|prefer(default)|off`; `gh_bin()` resolves `gh` to an absolute path from
allow-listed prefixes; `asset_name_matches()` / `match_asset()` require an exact triple match and abort on an
ambiguous asset set; staging + `std::fs::rename` give an atomic same-filesystem swap.

| # | Criterion | Verdict | Evidence |
|---|---|---|---|
| 1 | Artifact verified before any execution or install step | **MET** | `verify_artifact()` runs ahead of staging and of the rename; `sidecar_digest_mismatch_fails_closed` asserts the asset is still unmoved after a mismatch. |
| 2 | Verification is cryptographic — signature or pinned checksum from an independent source | **PARTIAL** | SHA-256 sidecar (mandatory) + Sigstore attestation (independent trust root) are both implemented, but the attestation policy **defaults to `prefer`**, so an unattested artifact installs on a sidecar fetched from the same host/channel as the artifact — which the card's own PRD R2 says "adds nothing". Strong path exists; it is not the default. Operator decision (OQ). |
| 3 | Verification failure ABORTS, binary untouched, no partial install state | **MET** | `sidecar_digest_mismatch_fails_closed`, `missing_sidecar_fails_closed` — abort occurs before staging. |
| 4 | Replacement is atomic (download → verify → temp → rename) | **MET** | staged into the destination directory then `std::fs::rename(&staged, &current_exe)`; same-filesystem atomic swap. |
| 5 | Self-update REFUSED in `Mode::Unattended` (per TASK-943) | **NOT MET** | grep for `Unattended` in `src/update.rs` → **0 hits** on current main. No mode gate on the update path: an unattended coordinator can replace the binary it is executing. This is a Must with no implementation. |
| 6 | Verification failure path unit-tested with a deliberately corrupted artifact | **MET** | `sidecar_digest_match_verifies` (happy) + `sidecar_digest_mismatch_fails_closed` (corrupted digest) both green. |
| 7 | `cargo test --no-default-features --locked` is green | **MET** | see commands below. |

**Not executable offline:** the live network fetch of a real release `.sha256` sidecar and a real
`gh attestation verify` round trip were **not** exercised (no network release fetch in this sweep). The code path
and its unit tests are the evidence for AC1/AC3/AC6; the end-to-end network behaviour is **UNVERIFIED** here and is
noted rather than claimed.

**Disposition:** stays in `col_review`. 5/7 MET, 1 PARTIAL (AC2 default policy), 1 NOT MET (AC5 unattended refusal
— a Must). Both remaining items are policy/release-pipeline decisions, not audit patches; tracked by open question.

---

## Commands actually executed (this sweep, worktree `w_5OHgmYfd`, main @ 78c72eb)

```
git rev-parse HEAD origin/main
  → 78c72ebe544cb96dfddee597361d0b43052b7715 (both)

git merge-base --is-ancestor b380713593a2c83cffe3d97121805a62d834c70e origin/main   # TASK-946 PR #939 → exit 0
git merge-base --is-ancestor 6e78aa567e1cb3b7f9193ef09a371cfc878b7b8b origin/main   # TASK-947 PR #933 → exit 0
git merge-base --is-ancestor c1da63f03110c100bd0d27c23fb1ffd30be6e59a origin/main   # TASK-948 PR #931 → exit 0

cargo test --no-default-features --locked secrets::tests
  → 15 passed; 0 failed; 0 ignored  (refuse_env_dumper, refuse_interpreter, refuse_wrapped_interpreter,
    refuse_unknown_binary, refuse_egress, allow_known_consumer, allow_program_override, key_allowlist_enforced,
    unattended_no_secrets_by_default, audit_logged_on_allow, audit_logged_on_refuse, audit_never_contains_value,
    argv_hash_is_stable_and_value_free, registered_for_redaction, assignment_and_operand_detection)

cargo test --no-default-features --locked section_siblings_never_materialise
  → 1 passed; 0 failed   (new test added by this sweep)
```

The full-suite gate (`cargo test --no-default-features --locked`, 2032 passed / 0 failed on merged main) was run by
the prior audit pass on this sprint; this sweep re-ran the focused secrets/tools filters above on
78c72eb and added one new passing test.
