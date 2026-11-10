//! Records the SHA-256 of the `libkrunfw` that the `microsandbox` build script embeds in `fbkd`
//! when the build supplies the runtime through `MSB_EMBED_ARTIFACTS_DIR`, as the Windows release
//! does (ADR 0061). `runtime::ensure` replaces an installed runtime whose library differs.

use std::path::Path;

use sha2::{Digest as _, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo::rerun-if-env-changed=MSB_EMBED_ARTIFACTS_DIR");
    println!("cargo::rerun-if-env-changed=MSB_EMBED_RUNTIME_BUNDLE_PATH");
    // microsandbox embeds a runtime bundle in preference to the directory, so the directory's
    // library isn't the embedded one then.
    if std::env::var_os("MSB_EMBED_RUNTIME_BUNDLE_PATH").is_some() {
        return Ok(());
    }
    let Some(directory) = std::env::var_os("MSB_EMBED_ARTIFACTS_DIR") else {
        return Ok(());
    };
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")?;
    let library = Path::new(&directory).join(microsandbox_utils::libkrunfw_filename(&target_os));
    println!("cargo::rerun-if-changed={}", library.display());
    let bytes = std::fs::read(&library)
        .map_err(|error| format!("failed to read {}: {error}", library.display()))?;
    let digest = Sha256::digest(bytes);
    println!("cargo::rustc-env=FBKD_EMBEDDED_LIBKRUNFW_SHA256={digest:x}");
    Ok(())
}
