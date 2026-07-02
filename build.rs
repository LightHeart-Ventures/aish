// build.rs: Generate version string with optional dev release tag injection
// When AISH_RELEASE_TAG is set (by the release-dev.yml workflow), format it
// into a user-visible version string.

fn main() {
    let base_version = env!("CARGO_PKG_VERSION");
    let release_tag = std::env::var("AISH_RELEASE_TAG").unwrap_or_default();

    let full_version = if release_tag.is_empty() || !release_tag.starts_with("dev-") {
        base_version.to_string()
    } else {
        // Format: "0.25.1 (dev snapshot dev-v0.26.0-dev.6)"
        format!("{} (dev snapshot {})", base_version, release_tag)
    };

    // Set a cfg! so we can use it via env!() in src/update.rs
    println!("cargo:rustc-env=AISH_VERSION_STRING={}", full_version);
}
