use std::path::Path;

/// Stamp the build with the repo-wide version (the top-level VERSION file is
/// the single source of truth). Falls back to the crate version for builds
/// outside the repo tree (e.g. a crate published on its own).
fn main() {
    let version_file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../VERSION");
    println!("cargo:rerun-if-changed={}", version_file.display());
    let version = std::fs::read_to_string(&version_file)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=LUMEN_VERSION={version}");
}
