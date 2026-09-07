//! Windows-only VERSIONINFO resource embedding.
//!
//! Windows Defender has twice quarantined released `zirv.exe` builds as a
//! trojan; an unsigned binary with no version resource at all is a much
//! stronger heuristic match than one that at least identifies itself the
//! way legitimate Windows software does. This does not fix the underlying
//! signing gap (out of scope -- needs an operator account, see
//! `docs/obsidian/Development/Known Issues.md`), but it is a real,
//! zero-cost mitigation.
//!
//! Guarded on `CARGO_CFG_TARGET_OS` rather than `#[cfg(windows)]` so a
//! cross-compile (building a Windows target from a non-Windows host, or
//! vice versa) is driven by the *target*, not the host running `cargo`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    embed_version_resource();
}

#[cfg(windows)]
fn embed_version_resource() {
    let version = env!("CARGO_PKG_VERSION");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set("ProductName", "zirv")
        .set(
            "FileDescription",
            "zirv CLI -- script runner and AI session supervisor",
        )
        .set("CompanyName", "Jonathan Solskov")
        .set("LegalCopyright", "Copyright (c) 2026 Jonathan Solskov")
        .set("OriginalFilename", "zirv.exe")
        .set("InternalName", "zirv")
        .set("FileVersion", version)
        .set("ProductVersion", version);

    if let Some(numeric) = numeric_version(version) {
        resource.set_version_info(winresource::VersionInfo::FILEVERSION, numeric);
        resource.set_version_info(winresource::VersionInfo::PRODUCTVERSION, numeric);
    }

    if let Err(error) = resource.compile() {
        // winresource needs rc.exe (MSVC toolchain) or windres (GNU
        // toolchain) on PATH. Fail the build loudly rather than silently
        // shipping an exe with no version resource -- that is exactly the
        // defect this file exists to close.
        panic!(
            "failed to compile the Windows VERSIONINFO resource (is rc.exe/windres available?): {error}"
        );
    }
}

/// Packs `major.minor.patch` into the `u64` `set_version_info` expects: four
/// packed `u16` fields, high to low, with the fourth (build) component fixed
/// at 0 since `CARGO_PKG_VERSION` carries only three numeric components.
#[cfg(windows)]
fn numeric_version(version: &str) -> Option<u64> {
    let mut parts = version.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    // `patch` may carry a `-pre`/`+build` suffix; keep only the leading
    // digits so a prerelease version still produces a valid resource.
    let patch_field = parts.next()?;
    let patch_digits: String = patch_field
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch: u64 = patch_digits.parse().ok()?;
    Some((major << 48) | (minor << 32) | (patch << 16))
}

// build.rs is compiled and run as its own standalone binary, outside the
// crate's normal test harness -- `cargo test`/`cargo nextest` never link or
// execute a `#[cfg(test)]` module placed here, so `numeric_version` is
// exercised indirectly instead, via the VersionInfo check in the PR
// checklist (`(Get-Item target/debug/zirv.exe).VersionInfo`).
