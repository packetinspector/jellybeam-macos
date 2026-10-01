//! Explicitly links `OpenGL.framework` into the `jellybeam` binary, and emits
//! the build-time metadata the About window's spec strip and BUILD label
//! read via `env!()` (`about.rs`).
//!
//! The OpenGL link is belt-and-suspenders: `player`'s own `build.rs`
//! already emits `cargo:rustc-link-lib=framework=OpenGL` on macOS (for its
//! headless CGL test harness), and Cargo propagates a dependency's link
//! directives to the final binary, so this crate would likely link
//! OpenGL.framework transitively either way. But `app` is the crate that
//! actually *resolves* GL entry points at runtime (`gl_video.rs`'s
//! `dlsym(RTLD_DEFAULT, ...)`, same pattern as `player/tests/common/mod.rs`),
//! so it depends on that symbol table being present in-process — worth
//! being explicit about rather than relying on an implicit transitive link.
//!
//! ## Build metadata
//!
//! `CARGO_PKG_VERSION` is already available to the crate's own code for
//! free (Cargo sets it for every crate, no build script involved) --
//! `about.rs` reads that one directly. Everything below is metadata Cargo
//! does NOT expose to the compiled binary on its own, so this script pulls
//! it from the build environment (`CARGO_CFG_*`, `git`, `rustc -V`) and
//! forwards each value via `cargo:rustc-env=NAME=value`, which is how a
//! build script hands a compile-time `env!("NAME")` its string.
//!
//! * `JELLYBEAM_BUILD` -- the commit count on `HEAD` (`git rev-list --count
//!   HEAD`), used as the About window's `BUILD nnnn` label: a strictly
//!   increasing integer that means "how many commits have landed," which
//!   is a more legible build counter than a hash for a human reading the
//!   About box. Falls back to `"0"` when `git` isn't on `PATH` or this
//!   isn't a git checkout at all (a source tarball build, say) -- never a
//!   hard build failure over a cosmetic label.
//! * `JELLYBEAM_RUSTC_VERSION` -- just the version token out of `rustc -V`
//!   (e.g. `1.82.0`), for the spec strip's `RUST` cell.
//! * `JELLYBEAM_TARGET_ARCH` -- `CARGO_CFG_TARGET_ARCH` verbatim (`aarch64`/
//!   `x86_64`); `about.rs` uppercases/relabels it for display (`ARM64`/
//!   `X86_64`) rather than this script hard-coding display strings into a
//!   value meant to be machine-readable.
//!
//! The About window's `© <year>` footer line is deliberately NOT baked in
//! here: a build-time year goes stale the moment a long-lived binary keeps
//! running past a New Year's Eve. `about.rs` reads the year from `chrono::
//! Utc::now()` at render time instead (already a normal, non-build,
//! dependency of this crate) -- see its own doc comment.
//!
//! `cargo:rerun-if-changed=.git/HEAD` (the workspace root's, resolved off
//! `CARGO_MANIFEST_DIR` since a build script's cwd is this crate's own
//! directory, not the workspace root) makes a `git commit`/checkout
//! re-trigger this script, so `JELLYBEAM_BUILD` doesn't go stale across a
//! session of commits without an unrelated file also changing.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=OpenGL");
    }
    println!("cargo:rerun-if-changed=build.rs");

    emit_build_metadata();
}

fn emit_build_metadata() {
    let build = git_commit_count().unwrap_or_else(|| "0".to_string());
    println!("cargo:rustc-env=JELLYBEAM_BUILD={build}");

    let rustc_version = rustc_version().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=JELLYBEAM_RUSTC_VERSION={rustc_version}");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=JELLYBEAM_TARGET_ARCH={arch}");

    // Re-run whenever the checked-out commit changes, so a build a few
    // commits after the last `cargo build` still picks up a fresh
    // `JELLYBEAM_BUILD` rather than reusing a cached one -- see the module
    // doc comment's "Build metadata" section.
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let git_head: PathBuf = [manifest_dir.as_str(), "..", "..", ".git", "HEAD"]
            .iter()
            .collect();
        println!("cargo:rerun-if-changed={}", git_head.display());
    }
}

/// `git rev-list --count HEAD`, run with this crate's own directory as the
/// working directory -- inside the repo, so `git` walks up to find `.git`
/// on its own; no explicit `-C` needed. `None` on any failure (git
/// missing, not a git checkout, detached weirdness) -- the caller treats
/// that as "unknown," never a build failure.
fn git_commit_count() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let count = String::from_utf8(output.stdout).ok()?;
    let count = count.trim();
    (!count.is_empty()).then(|| count.to_string())
}

/// Just the version token out of `rustc -V`'s first line (e.g.
/// `"rustc 1.82.0 (f6e511eec 2024-10-15)"` -> `"1.82.0"`). Reads `$RUSTC`
/// (Cargo always sets it for build scripts) rather than assuming a bare
/// `rustc` resolves on `PATH` -- the same rustc actually building this
/// crate, not whatever a shell's default toolchain happens to be.
fn rustc_version() -> Option<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(rustc).arg("-V").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    text.split_whitespace().nth(1).map(str::to_string)
}
