# aish Release Channels

aish ships on **three release channels**. A channel decides which GitHub
Release `:update` (and the startup update check) tracks. All three funnel into
the *same* download → checksum-verify → apply path in `src/update.rs`; only the
*discovery* of the target release differs.

| Channel | Env value | Tag format | Marked "latest"? | Trigger | Audience |
|---------|-----------|------------|:---------------:|---------|----------|
| **prod** (default) | `prod` / unset | `v{semver}` (e.g. `v0.24.0`) | ✅ yes | manual dispatch of `release-prod.yml` | everyone — stable |
| **dev** | `dev` | `dev-v{next}-dev.{n}` (e.g. `dev-v0.24.0-dev.3`) | ❌ pre-release | nightly cron + dispatch (`release-dev.yml`) | early adopters |
| **ci** | `ci` | `ci-{run}-{sha8}` (e.g. `ci-482-a1b2c3d4`) | ❌ pre-release | every push to `main` (`release-ci.yml`) | maintainers / dogfooders |

## Selecting a channel

Set the `AISH_UPDATE_CHANNEL` environment variable before launching aish:

```sh
# Stable (default — you can also just leave it unset)
export AISH_UPDATE_CHANNEL=prod

# Nightly pre-releases
export AISH_UPDATE_CHANNEL=dev

# Bleeding edge — a build per merge to main
export AISH_UPDATE_CHANNEL=ci
```

Accepted values are case-insensitive; `stable`/`release` alias `prod` and
`nightly` aliases `dev`. Anything unrecognised (or unset) falls back to
**prod**, so existing users need to do nothing.

Check the active channel from inside the shell:

```
:channel
update channel → prod (stable v{semver} releases marked latest)
```

> `:channel` is **read-only** for now (a stub, per the initial rollout). It
> reports the channel resolved from `AISH_UPDATE_CHANNEL`; to switch, set the
> env var and relaunch.

## How discovery works per channel

- **prod** — `gh release view` with no tag returns the repo's "latest"
  published release. This is the original, unchanged behaviour (full backward
  compatibility).
- **dev / ci** — dev and ci tags are *not* strict semver, so the "latest"
  pointer can't find them. Instead aish runs `gh release list` and
  client-side filters by the channel's tag prefix (`dev-` / `ci-`), taking the
  newest match (the list is newest-first).

## `:update` on each channel

```sh
# prod: offers the newest stable vX.Y.Z when it's newer than the running build
AISH_UPDATE_CHANNEL=prod aish
:update
# → aish is up to date (v0.24.0)   — or —   Update available: v0.25.0

# dev: offers the newest dev-* pre-release
AISH_UPDATE_CHANNEL=dev aish
:update
# → Update available: dev-v0.25.0-dev.7

# ci: offers the newest ci-* build off main
AISH_UPDATE_CHANNEL=ci aish
:update
# → Update available: ci-503-9f8e7d6c
```

### A note on dev/ci re-prompting

The running binary always reports its **Cargo.toml** version (e.g. `0.24.0`) —
it has no way to record which dev/ci *tag* it was installed from. Because
dev/ci tags aren't semver, the newer-check falls back to a string comparison,
so on the dev/ci channels `:update` may keep offering the newest pre-release
even right after you install it. That's expected for the pre-release tracks;
install when you want the tip, ignore the prompt otherwise. The **prod** channel
does not have this behaviour — its semver compare is exact.

## Retention / pruning

High-frequency channels self-prune to avoid unbounded release accumulation:

- **ci** keeps the **10** most recent `ci-*` releases; older ones (and their
  tags) are deleted at the end of each `release-ci.yml` run.
- **dev** keeps the **10** most recent `dev-*` releases likewise.
- **prod** releases are never auto-pruned.

## Migration path for existing users

Nothing to do. Before this change `:update` always tracked the latest stable
release; that is exactly what the **prod** channel does, and prod is the default
when `AISH_UPDATE_CHANNEL` is unset. Opt into `dev` or `ci` only if you want
pre-release builds.

## Release engineering constraints (SRE)

These workflows honour the aish SRE runbook:

- **Workflows mint every tag** — no hand-pushed tags. `softprops/action-gh-release`
  creates the tag with `target_commitish: github.sha`. Because it runs under the
  default `GITHUB_TOKEN`, tag creation does **not** retrigger the legacy
  tag-push `release.yml` (GitHub suppresses workflow recursion from the default
  token), so there's no double build.
- **Immutable releases** — `release-prod.yml` fails fast if a *published*
  release already exists for the computed tag (a published release can't receive
  assets, which would strand `:update`). Pre-create a **draft** if you must
  stage one.
- **Pre-release flag** on dev/ci guarantees they never show as "latest", so
  prod-channel users are never pushed a pre-release.
- Every asset ships a `SHA256` sidecar (plus a roll-up `SHA256SUMS`); `:update`
  verifies the download against it before applying.
