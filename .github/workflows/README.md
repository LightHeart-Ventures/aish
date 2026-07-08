# Release Workflow Architecture

This directory houses the release channel workflows for aish. The architecture separates **trigger** concerns from **build** concerns to reduce duplication and maintain a single source of truth for per-platform compilation.

## Release Channels

### `release-production.yml`

**Purpose:** Manual, controlled releases for end-users (the only public release channel).

**Triggers:**
1. **Tag push** (legacy, still active): `git tag v0.23.1 && git push origin v0.23.1`
   - Automatically detects version from tag name.
   - Validates against Cargo.toml.
2. **Workflow dispatch** (modern, recommended):
   - GitHub UI: Actions > Release (Production) > Run workflow.
   - Prompts for version (e.g., `0.23.1`) and optional release notes.
   - Creates the tag on origin/main before building.

**Outputs:**
- GitHub Release with per-platform binaries (macOS x86_64 + arm64, Linux x86_64).
- SHA256SUMS file for integrity verification.
- Tagged with `:update` command expectations.

**SLA:** Manual trigger, runs in ~5 minutes (cross-compilation for three targets).

---

### `release-ci-cd.yml`

**Purpose:** Continuous delivery of bleeding-edge builds from every commit to main.

**Triggers:**
- `push: branches: main` — runs on every merge to main.

**Outputs:**
- GitHub Release tagged with `ci-{run_number}-{short_sha}` (e.g., `ci-1234-a1b2c3d`).
- Marked prerelease so it doesn't override production.
- Latest artifacts available for internal testing and CI integration tests.

**SLA:** ~5 minutes per run (same as production).

---

### `release-dev.yml`

**Purpose:** Rapid feedback loop during active development; tags on feature branches.

**Triggers:**
- `push: branches: dev/*` — runs when a feature branch matches the pattern.

**Outputs:**
- GitHub Release tagged with `dev-{branch_name}-{short_sha}`.
- Marked prerelease.
- One artifact per active feature branch; older artifacts are garbage-collected.

**SLA:** ~5 minutes per run.

---

## Build Logic: `build-release-binary.yml`

A **reusable workflow** called by all three release channels to eliminate duplication:

- Installs Rust toolchain and target.
- Caches cargo registry & compiled artifacts per (OS, target, Cargo.lock).
- Builds with `--release --features local` (or configurable features).
- Packages the binary + SHA256 sidecar.
- Uploads as a GitHub Actions artifact (used by the calling release workflow).

**Why reusable?** Each release channel orchestrates its own tag validation, version detection, and release semantics, but the **per-target build** steps are identical. A reusable workflow centralizes the Rust-specific logic so changes to the build command, features, or cache strategy propagate to all channels in one edit.

---

## Workflow Design Decisions

### Single Tag vs. Multiple Tags

**Decision:** All release channels use standard semver tags (`v0.23.1`, `ci-1234-a1b2c3d`, `dev-feature-foo-a1b2c3d`).

**Why:** GitHub Release is immutable once published with assets. Pre-publishing a bare release (without assets) causes the asset-upload step to fail with "target_commitish cannot be changed when release is immutable." By using a single tag (matched by the workflow, or created by dispatch), we ensure the workflow publishes the release with assets in one shot.

### Checkout Ref: Tag vs. origin/main

- **production:** Checks out `ref: <tag>` (the exact commit tagged for release).
- **ci-cd & dev:** Checks out `ref: origin/main` (or the feature branch) to build the latest HEAD on that branch, even if a tag has been created.

### Dispatch Input: Version Only

**Why no "branch" input for production releases?**
- Production releases must be traceable: `git tag v0.23.1 -> commit abc123 -> release assets`.
- Allowing an arbitrary branch input would obscure the link (tagging a branch-tip commit instead of a named tag adds ambiguity).
- Dispatch is a convenience for users; the enforce-on-main pattern (with a single origin/main checkout) prevents accidental dev releases.

---

## Common Operations

### Releasing to Production

**Option 1: Tag push (CLI)**
```bash
# Bump Cargo.toml version to 0.23.1, commit, and push the tag.
git tag v0.23.1
git push origin v0.23.1
```

**Option 2: Workflow dispatch (UI)**
- Go to GitHub Actions > **Release (Production)** > Run workflow.
- Enter version `0.23.1` (without the `v` prefix).
- (Optional) Add release notes.
- Click **Run workflow**.

Both paths are safe and produce identical results. Choose dispatch for clarity and to prevent tag/Cargo.toml drift.

### Monitoring a Release

- **GitHub Actions:** Watch the workflow run (3 jobs: verify → build → release).
- **GitHub Releases:** See the published release with assets once the workflow completes.

### Debugging a Failed Release

1. **Tag/version mismatch:**
   - Check the verify-version job output.
   - If Cargo.toml version differs from the tag, bump Cargo.toml and re-push the tag.

2. **Build failure:**
   - Check the build job logs (one per target).
   - Common fixes: missing toolchain target, cache stale, Cargo.lock out of sync.

3. **Release asset upload failure:**
   - Verify no pre-published release exists for the tag.
   - If one does, delete it and retry (or create as a draft and let the workflow publish it).

---

## Future Enhancements

- **Checksum validation on pull:** Implement `:update` command to fetch and verify binaries against SHA256SUMS.
- **Signature verification:** Add GPG signing for production releases (require per-release passphrase).
- **Artifact retention:** Archive older CI/dev releases in S3; keep only the latest 5 per channel on GitHub.
- **Multi-provider build:** Support building with and without the `local` feature (Claude-only variant for Docker / restricted environments).
