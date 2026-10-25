// build.rs: bake the release provenance into the binary.
//
// The ONE fact we need at runtime is the release tag this build came from —
// `AISH_RELEASE_TAG`, set by the release workflows (`v0.71.2`,
// `dev-v0.71.2-dev.6`, `ci-<run>-<sha>`) and empty for a local `cargo build`.
// Formatting it for humans is deliberately NOT done here: `src/version_fmt.rs`
// owns that, where it is pure and unit-tested. build.rs only forwards the raw
// tag so a formatting change never requires reasoning about build scripts.

fn main() {
    let release_tag = std::env::var("AISH_RELEASE_TAG").unwrap_or_default();

    // Always emitted (empty when unset) so `env!("AISH_RELEASE_TAG")` in
    // version_fmt.rs compiles in every build configuration.
    println!("cargo:rustc-env=AISH_RELEASE_TAG={release_tag}");

    // Without this, a changed tag wouldn't trigger a rebuild and the binary
    // would keep reporting the previous build's provenance.
    println!("cargo:rerun-if-env-changed=AISH_RELEASE_TAG");
}
