# Release Guide

This document covers aish release procedures, channels, and recovery.

---

## Quick Start

### For users: choosing a release channel

```bash
export AISH_UPDATE_CHANNEL=prod    # Stable (default)
export AISH_UPDATE_CHANNEL=dev     # Nightly development
export AISH_UPDATE_CHANNEL=ci      # Latest CI snapshot (internal testing)

:update                             # Pull latest from your channel
:update prod                        # Override to prod, just once
```

### For maintainers: cutting a release

1. Merge a `release/vX.Y.Z` PR (bumps `Cargo.toml` version)
2. Tag the merge commit and push only the tag:
   ```bash
   git tag vX.Y.Z <merge-commit-sha>
   git push origin vX.Y.Z
   ```
3. The `Release` workflow automatically builds and publishes. **Do NOT manually create/publish the release in GitHub.**

---

## Release Channels

aish ships on three independent channels so you can choose between stability and bleeding-edge development.

| Channel | Cadence | Tag Format | Pre-release? | Use Case |
|---------|---------|-----------|-------------|----------|
| **prod** (default) | Manual | `v{semver}` (e.g. `v0.23.0`) | No | Stable, production-ready. Default for `:update`. |
| **dev** | Nightly + manual | `dev-v{next}-dev.{n}` | Yes | Daily curated snapshot. More stable than CI. |
| **ci** | Every main commit | `ci-{run_number}-{short_sha}` | Yes | Internal testing. Latest code, most unstable. |

### Selecting a channel

#### Environment Variable (Permanent)

```bash
# Add to ~/.bashrc or ~/.profile
export AISH_UPDATE_CHANNEL=dev

# Restart shell or source it
source ~/.bashrc
```

#### One-off Override

```bash
:update dev     # Pull nightly, just this once
:update prod    # Force stable, ignoring an exported dev/ci channel
:update ci      # Pull the latest CI snapshot
```

Valid aliases: `stable` / `release` → `prod`, `nightly` → `dev`.

#### Checking your current channel

```bash
echo $AISH_UPDATE_CHANNEL   # prints 'dev', 'ci', 'prod', or empty (defaults to prod)
```

### How `:update` discovers releases

- **prod**: `gh release view` (latest published, non-prerelease release)
- **dev/ci**: `gh release list` + tag-prefix filter (finds newest matching the channel's pattern, includes pre-releases)

Once discovered, all channels use the same download/verify/swap mechanism.

---

## Release Procedures

### Production Release (Manual)

A maintainer triggers the release for a new stable version.

**Procedure:**

1. Create a `release/vX.Y.Z` branch off `main`
2. Bump `version` in `Cargo.toml` (and refresh `Cargo.lock`)
3. Update CHANGELOG (if applicable)
4. Open a PR, merge to `main`
5. Tag the merge commit:
   ```bash
   git tag vX.Y.Z <merge-commit-sha>
   git push origin vX.Y.Z
   ```
6. The `.github/workflows/release.yml` workflow runs automatically:
   - Verifies `Cargo.toml` version matches tag
   - Builds for macOS (x86_64 + arm64) and Linux (x86_64)
   - Generates checksums
   - Creates and publishes a GitHub Release with all binaries attached
7. **Do NOT manually create or publish the release.** The workflow handles it.

**Why?** GitHub releases are now **immutable** — once published, assets cannot be added. Pre-publishing the release prevents the workflow from attaching binaries, breaking `:update` for that version.

### Dev Release (Nightly)

Automatically triggered at 04:00 UTC daily, or manually via `workflow_dispatch`.

**Procedure (if manual):**

```bash
gh workflow run release-dev.yml --repo LightHeart-Ventures/aish
```

**Workflow does:**
1. Extracts the next minor version from `Cargo.toml` (e.g. 0.24.0 → 0.25.0)
2. Creates a pre-release tag `dev-v0.25.0-dev.{run_number}`
3. Builds and publishes
4. Auto-prunes old dev releases (keeps 5 newest)

### CI Release (Per-Commit)

Auto-triggered on every main push.

**Workflow does:**
1. Creates a pre-release tag `ci-{run_number}-{short_sha}`
2. Builds and publishes
3. Auto-prunes old CI releases (keeps 10 newest)

---

## Release Notes & Drafts

To hand-write release notes, create a **draft** release before pushing the tag:

```bash
gh release create vX.Y.Z --draft --notes "
## What's New
- Feature A
- Bug fix B

## Breaking Changes
- None

## Contributors
@alice @bob
"

git push origin vX.Y.Z
```

Draft releases remain mutable — the workflow will upload assets and publish it.

---

## Recovering a Broken Release

If a release ships with no assets (due to immutability), you have two options:

### Option 1: Re-cut the same version (if the broken release can be deleted)

```bash
gh release delete vX.Y.Z --repo LightHeart-Ventures/aish --yes
git push --delete origin vX.Y.Z
git tag -d vX.Y.Z
git tag vX.Y.Z <merge-commit-sha>
git push origin vX.Y.Z
```

### Option 2: Bump forward to vX.Y.(Z+1) (preferred if broken release is locked)

```bash
# Create release/vX.Y.(Z+1) PR, merge, and release normally
git tag vX.Y.(Z+1) <merge-commit-sha>
git push origin vX.Y.(Z+1)
```

---

## Troubleshooting

### `:update` isn't finding my dev build

1. Verify the env var: `echo $AISH_UPDATE_CHANNEL`
2. Check releases exist: `gh release list --repo LightHeart-Ventures/aish | grep dev-`
3. Trigger nightly manually:
   ```bash
   gh workflow run release-dev.yml --repo LightHeart-Ventures/aish
   ```

### I accidentally set the wrong channel

```bash
unset AISH_UPDATE_CHANNEL          # Default back to prod
aish -c ':update'
```

Or explicitly set it:

```bash
export AISH_UPDATE_CHANNEL=prod
aish -c ':update'
```

### A dev/ci tag reports the wrong version

Dev and CI releases are pre-releases built from intermediate states. The binary's `Cargo.toml` is correct, but the tag format is non-semver. If you need strict version matching, switch to `prod`:

```bash
unset AISH_UPDATE_CHANNEL
aish -c ':update'
```

### Security patch released on `prod`, but I'm on `dev`

The dev channel will get the patch when the nightly build runs or a new dev release is cut from main. If you need it immediately:

```bash
unset AISH_UPDATE_CHANNEL
aish -c ':update'
```

---

## FAQ

**Q: Can I run two aish binaries tracking different channels?**

Yes. Install to different locations and set the env var per-shell:

```bash
# Shell A: prod
~/.local/bin/aish

# Shell B: dev
export AISH_UPDATE_CHANNEL=dev
aish -c ':update'
```

**Q: Can I create a custom channel?**

Not yet — channels are hardcoded (`prod`, `dev`, `ci`). A future extension could support `AISH_UPDATE_CHANNEL_PREFIX` for forks.

**Q: Version numbering across channels?**

- **prod**: `v0.23.0 → v0.24.0 → v0.25.0` (stable semver)
- **dev**: `dev-v0.25.0-dev.1, dev-v0.25.0-dev.2, …` (pre-release per cycle)
- **ci**: `ci-12345-a1b2c3d4` (run number + short SHA)

All three are mutually exclusive — you won't see a dev tag if you're on prod.

---

## Workflows Overview

**Workflow Files:**
- `.github/workflows/release.yml` — Production releases (manual)
- `.github/workflows/release-dev.yml` — Dev releases (nightly + manual)
- `.github/workflows/release-ci.yml` — CI releases (per-commit)

**Key Principle:** The release workflow creates the GitHub Release and publishes it in one transaction. No manual post-creation steps.

---

## See Also

- [docs/ARCHITECTURE.md](./ARCHITECTURE.md) — System overview
- [docs/INDEX.md](./INDEX.md) — Documentation navigator
