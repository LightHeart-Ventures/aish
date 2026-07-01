# Multi-Release Channels

aish supports three independent release channels so you can choose between stability and bleeding-edge development.

## Channels

| Channel | Release cadence | Tag format | Pre-release? | Use case |
|---|---|---|---|---|
| **prod** (default) | Manual via workflow dispatch | `v{semver}` (e.g. `v0.23.0`) | No — marked `latest` | Stable, production-ready builds. Default for `:update`. |
| **dev** | Nightly + manual | `dev-v{next}-dev.{n}` | Yes | Daily curated development snapshot. More stable than CI but bleeding-edge. |
| **ci** | Every main commit | `ci-{run_number}-{short_sha}` | Yes | Internal testing only. Latest code, most unstable. Auto-published on every merge. |

## Selecting a Channel

The primary mechanism is the `AISH_UPDATE_CHANNEL` environment variable:

```bash
# Stay on stable production releases (the default)
export AISH_UPDATE_CHANNEL=prod

# Track nightly development builds
export AISH_UPDATE_CHANNEL=dev

# Track the latest CI snapshot (internal testing only)
export AISH_UPDATE_CHANNEL=ci
```

When unset or unrecognized, `:update` defaults to `prod` (stable).

### Setting the channel permanently

Add the export to your shell profile:

```bash
echo 'export AISH_UPDATE_CHANNEL=dev' >> ~/.bashrc
echo 'export AISH_UPDATE_CHANNEL=dev' >> ~/.profile
```

Then restart your shell or source the profile:

```bash
source ~/.bashrc
```

### Checking your current channel

The environment variable is read at startup:

```bash
echo $AISH_UPDATE_CHANNEL   # prints 'dev', 'ci', 'prod', or (empty for default)
```

## How It Works

The `:update` command uses `gh release` to discover and download the latest release for your channel:

- **prod**: `gh release view` (returns the repo's latest published, non-prerelease release)
- **dev/ci**: `gh release list` + client-side tag-prefix filter (finds newest release matching the channel's tag pattern, includes pre-releases)

Once a release is discovered, all three channels use the same download/verify/swap path. The asset format and binary self-reporting (`:version`) are identical across channels.

## Examples

### Switch to dev builds

```bash
export AISH_UPDATE_CHANNEL=dev
aish -c ':update'
```

Output:
```
[dev] checking for updates …
Found dev-v0.24.0-dev.42 (newer than 0.23.0) — installing…
✓ downloaded aish-aarch64-apple-darwin (28.5 MB)
installing …
✓ installed to ~/.local/bin/aish
Installed aish dev-v0.24.0-dev.42. Restart the shell to apply changes.
```

### Downgrade back to stable

```bash
export AISH_UPDATE_CHANNEL=prod
aish -c ':update'
```

The downgrade is allowed because the semver logic compares `(0, 23, 0)` from `v0.23.0` against the dev tag's numeric version — a lower semver returns a genuine update.

### Monitor CI automatically (advanced)

```bash
export AISH_UPDATE_CHANNEL=ci
# Add to a cron job or systemd timer to pull the latest every morning
aish -c ':update'
```

## Backward Compatibility

Existing users are **unaffected** — the default channel is `prod`, which is unchanged from the original `:update` behavior. Multi-channel support is entirely opt-in via the `AISH_UPDATE_CHANNEL` env var.

### Release numbering

- **prod** keeps the stable `v{semver}` sequence (e.g. v0.23.0 → v0.24.0 → v0.25.0).
- **dev** increments the minor version for a dev cycle (e.g. `dev-v0.24.0-dev.1`, `dev-v0.24.0-dev.2`, …).
- **ci** uses the GitHub Actions run number + short commit SHA (e.g. `ci-12345-a1b2c3d4`).

All three are mutually exclusive — a user on `prod` will never see a dev/ci tag in `:update`, and vice versa.

## Release Workflows

### Production Release (manual)

A maintainer triggers the `Release (Production)` workflow with a version number:

```bash
gh workflow run release-prod.yml -f version=0.24.0
```

The workflow:
1. Verifies the version matches `Cargo.toml`
2. Builds for all platforms
3. Creates a tagged release marked `latest`

### Dev Release (nightly)

Triggered automatically at 04:00 UTC daily (or manually via `workflow_dispatch`).

The workflow:
1. Extracts the next minor version from `Cargo.toml` (e.g. 0.24.0 → 0.25.0)
2. Creates a pre-release tag `dev-v0.25.0-dev.{run_number}`
3. Builds and publishes the release
4. Automatically prunes old dev releases (keeps the 5 newest)

### CI Release (per-commit)

Auto-triggered on every `main` push. Creates a pre-release tag `ci-{run_number}-{short_sha}` and publishes.

Automatically prunes old CI releases (keeps the 10 newest).

## Troubleshooting

### `:update` isn't finding my dev build

1. Verify the env var is set: `echo $AISH_UPDATE_CHANNEL`
2. Check that a dev release exists: `gh release list --repo LightHeart-Ventures/aish | grep dev-`
3. If no dev releases exist, wait for the nightly workflow or trigger it manually:
   ```bash
   gh workflow run release-dev.yml --repo LightHeart-Ventures/aish
   ```

### I want to track prod but I set `AISH_UPDATE_CHANNEL=dev` by mistake

Unset the env var (aish defaults to prod):

```bash
unset AISH_UPDATE_CHANNEL
aish -c ':update'
```

Or explicitly set it back:

```bash
export AISH_UPDATE_CHANNEL=prod
```

### A dev/ci tag reports the wrong version (e.g. `:version` shows an old number)

Dev and CI releases are pre-releases and may be built from intermediate states. The binary's `Cargo.toml` version is correct, but the release tag format is non-semver. This is expected — revert to `prod` if you need strict version matching.

## FAQ

**Q: Can I have two different aish binaries tracking different channels?**

Yes. Install aish to a custom location, then set `AISH_UPDATE_CHANNEL` per-shell:

```bash
# Shell A: prod
~/.local/bin/aish

# Shell B: dev
~/.local/bin/aish-dev  # compiled separately, or via symlink + setting the env var
export AISH_UPDATE_CHANNEL=dev
aish -c ':update'
```

**Q: What if I'm on dev and prod releases a security patch?**

The dev channel will eventually include the patch when the nightly build runs or a new dev release is cut from a main commit. If you need it immediately, switch back to prod:

```bash
unset AISH_UPDATE_CHANNEL
aish -c ':update'
```

**Q: Why does the CI channel exist if it's unstable?**

Internal testing and rapid iteration. You can also use it to test features before they're cut into a dev build.

**Q: Can I make my own channel with a custom tag prefix?**

Not yet — channels are hardcoded (`prod`, `dev`, `ci`). A future extension could accept `AISH_UPDATE_CHANNEL_PREFIX` for forks, but that's out of scope today.
