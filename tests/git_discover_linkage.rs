//! aish ↔ git-discover linkage test.
//!
//! Proves the `git-discover` crate is wired into aish and can answer "what repo
//! am I working with right now?" against the live aish checkout this test runs
//! in. Skips gracefully outside a git checkout (e.g. a source tarball build).

use std::path::Path;

#[test]
fn aish_can_discover_its_own_repo() {
    // CARGO_MANIFEST_DIR is the aish crate root — inside the aish checkout when
    // tests run from a git worktree.
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    match git_discover::discover(manifest_dir) {
        Some(info) => {
            // Root must be a real path and the repo_key must always be present.
            assert!(info.root.exists(), "discovered root should exist");
            assert!(!info.repo_key.is_empty(), "repo_key is always populated");
            // Trunk is resolved, never empty.
            assert!(!info.trunk.is_empty(), "trunk resolves to a branch name");
            // A branch OR an explicit detached-HEAD flag — never both empty.
            assert!(
                info.branch.is_some() || info.detached,
                "either on a branch or detached"
            );
            // When the origin remote is the aish repo, identity should resolve.
            if let Some(slug) = &info.slug {
                assert!(slug.contains('/'), "slug is owner/repo shaped: {slug}");
            }
        }
        None => {
            // Building outside a git checkout (tarball / vendored) — nothing to
            // assert, but the crate linked and ran.
            eprintln!("manifest dir is not a git repo — skipping repo assertions");
        }
    }
}
