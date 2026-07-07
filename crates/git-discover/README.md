# git-discover

Zero-dependency git repository **discovery** for aish — "what repo am I working
with right now?"

Point it at a directory and it detects the checkout by shelling cheap,
read-only `git` probes, returning a `RepoInfo`:

| field | meaning |
|---|---|
| `root` | worktree root (`git rev-parse --show-toplevel`) |
| `remote_url` / `host` / `owner` / `repo` / `slug` | parsed from the `origin` remote |
| `repo_key` | stable `owner--repo` key (or basename+hash fallback) |
| `branch` / `detached` | current branch, or detached-HEAD flag |
| `head` / `short_head` | HEAD commit sha |
| `trunk` / `on_trunk` | resolved trunk branch (never hard-coded `main`) and whether HEAD sits on it |
| `dirty` | uncommitted/untracked changes present |
| `is_linked_worktree` | this is a `git worktree add` checkout, not the primary tree |

Discovery only **observes** — nothing here mutates the working tree, and any
git error degrades to a conservative `None`/`false` instead of panicking.

## Library

```rust
if let Some(info) = git_discover::discover_here() {
    println!("{}", info.summary());   // acme/widget on feature (trunk main) — clean
    println!("{}", info.to_json());   // full RepoInfo as JSON (no serde)
}
```

Pure helpers (`parse_remote`, `repo_key_from_remote`, `sanitize_repo_key`,
`fallback_repo_key`) are forge-agnostic — GitHub, GitLab (incl. subgroups),
Bitbucket, and self-hosted hosts across https / ssh / `git://` / scp-like
(`git@host:owner/repo`) URL shapes — and unit-tested without spawning git.

## CLI

```
git-discover [PATH] [--json|-j] [--key|-k] [--quiet|-q]
```

Exit `0` inside a repo, `1` otherwise — usable as a predicate:

```sh
git-discover --quiet && echo "in a repo"
git-discover --key            # -> LightHeart-Ventures--aish
git-discover --json | jq .trunk
```
