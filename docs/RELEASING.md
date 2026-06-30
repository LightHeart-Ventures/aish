# Releasing aish

`aish` ships prebuilt binaries as GitHub Release assets. The `:update` command
(`src/update.rs`) discovers the latest tag with `gh release view`, downloads the
per-platform binary, and verifies it against its sibling `.sha256`. **A release
with no assets silently breaks `:update`** — there is nothing to download.

Releases are produced by `.github/workflows/release.yml`, which triggers on any
pushed `v*` tag. It builds binaries for macOS (x86_64 + arm64) and Linux
(x86_64), generates checksums, and publishes a GitHub Release with every binary
attached — all in one shot via `softprops/action-gh-release@v2`.

## The one rule: let the workflow create the release

**Do NOT create/publish the GitHub Release yourself.** Just push the tag and let
the workflow do everything.

### Why — GitHub immutable releases

GitHub now publishes releases as **immutable**: the moment a release is
published it is frozen. You cannot change its metadata, and you cannot attach
assets afterwards (`HTTP 422: Cannot upload assets to an immutable release`).

If a bare release is pre-published out-of-band (e.g. a local script running
`gh release create v0.X.Y --notes "…"` right after the tag push), the workflow's
`publish release` job finds that frozen release and cannot attach the binaries.
It fails ~4 minutes in with:

```
Validation Failed: {"resource":"Release","code":"custom","field":"target_commitish",
"message":"target_commitish cannot be changed when release is immutable"}
```

The release then exists **with zero assets**, and `:update` is broken for that
version. (This is exactly what happened to v0.18.3 in run 28423477622.) The
`verify-version` job now guards against this and fails fast with a clear message,
but the real fix is to not pre-publish.

## Correct release procedure

1. Bump `version` in `Cargo.toml` (and refresh `Cargo.lock`), open a
   `release/vX.Y.Z` PR, and merge it. The tag must equal the `Cargo.toml`
   version — the `verify-version` job enforces this.
2. Tag the merge commit and push **only the tag**:
   ```sh
   git tag vX.Y.Z <merge-commit-sha>
   git push origin vX.Y.Z
   ```
3. Watch the `Release` workflow. It creates and publishes the release with all
   binaries attached. Done — do not touch the release in the GitHub UI/CLI.

If you want to hand-write release notes, create a **draft** release before
pushing the tag (drafts stay mutable; the workflow uploads assets and publishes
it):

```sh
gh release create vX.Y.Z --draft --notes "…"   # DRAFT, not published
git push origin vX.Y.Z
```

## Recovering a release that shipped with no assets

An immutable, already-published release cannot be repaired in place (assets
can't be added). Pick one:

- **Re-cut the same version** (only if the release can be deleted): delete the
  release and the tag, then re-push the tag so the workflow recreates it with
  assets:
  ```sh
  gh release delete vX.Y.Z --repo <owner>/<repo> --yes
  git push --delete origin vX.Y.Z
  git tag -d vX.Y.Z
  git tag vX.Y.Z <merge-commit-sha>
  git push origin vX.Y.Z
  ```
- **Bump forward** to vX.Y.(Z+1) and release normally (preferred if the broken
  release can't be deleted or has already been consumed).
