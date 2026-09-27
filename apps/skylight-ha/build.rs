//! Bakes the build's own identity into the binary, for the in-app updater.
//!
//! Three values, all consumed by `src/update.rs`:
//!
//! - `SKYLIGHT_VERSION` -- the workspace package version. This is the value
//!   the updater compares against the published `manifest.json`, and the one
//!   CI cross-checks against the pushed git tag (see
//!   `.github/workflows/build-pi.yml`), so that "forgot to bump Cargo.toml"
//!   is a red CI run rather than a device that never sees the release it just
//!   published.
//! - `SKYLIGHT_GIT_SHA` -- for the read-only build line in Settings. Purely
//!   informational; nothing compares it.
//! - `SKYLIGHT_BUILD_EPOCH` -- Unix seconds at build time, used *only* as a
//!   clock-sanity floor. This board has no RTC and boots at the kernel epoch
//!   (1970) with NTP correcting it seconds-to-a-minute later, and TLS
//!   certificate validation fails outright against a 1970 clock. A binary
//!   cannot legitimately be running before it was built, so
//!   `now < SKYLIGHT_BUILD_EPOCH` is a cheap, deterministic "the clock hasn't
//!   been corrected yet, don't bother trying the network" test. See
//!   `update::clock_is_sane`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    println!("cargo:rustc-env=SKYLIGHT_VERSION={version}");
    println!("cargo:rustc-env=SKYLIGHT_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=SKYLIGHT_BUILD_EPOCH={}", build_epoch());

    // Both are read below; without these, cargo would happily serve a cached
    // build script result after they changed.
    println!("cargo:rerun-if-env-changed=SKYLIGHT_GIT_SHA");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    watch_git_head();
}

/// The short commit this was built from.
///
/// An explicit `SKYLIGHT_GIT_SHA` in the environment wins, because the real
/// aarch64 builds happen inside a QEMU-emulated Alpine container that has no
/// `git` installed and would hit git's "dubious ownership" check on the
/// bind-mounted workspace even if it did. CI passes `github.sha` in that way
/// (see the workflow), so the released binary still knows its own commit
/// without adding a package to the container or relaxing git's safe.directory
/// rules. Falls back to asking `git` directly (the normal dev-machine path),
/// then to `"unknown"` -- never a hard build failure, since nothing
/// load-bearing depends on this value.
fn git_sha() -> String {
    if let Ok(from_env) = std::env::var("SKYLIGHT_GIT_SHA") {
        let trimmed = from_env.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let Some(sha) = run_git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".to_string();
    };
    // `--untracked-files=no`: an untracked scratch file in the working tree
    // says nothing about what actually got compiled.
    let dirty = run_git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

fn run_git(args: &[&str]) -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = Command::new("git").args(args).current_dir(manifest_dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build timestamp, honouring `SOURCE_DATE_EPOCH` when it's set so a
/// reproducible-build environment can pin it. Note this is the epoch of the
/// last time *this build script* ran, not of the final link -- which is
/// exactly right for its one purpose (a lower bound on "the clock is
/// plausible"), and means an incremental rebuild that doesn't re-run the
/// script keeps the older, safely-lower value.
fn build_epoch() -> u64 {
    if let Ok(raw) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            return parsed;
        }
    }
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Re-run when `HEAD` moves, so the baked-in SHA doesn't go stale across
/// branch switches and commits.
///
/// Only emits `rerun-if-changed` for paths that actually exist: cargo treats
/// a missing declared path as "changed", which would force the build script
/// to re-run on *every* build (and, because `SKYLIGHT_BUILD_EPOCH` is `now`,
/// force a recompile of the whole crate each time) in a source tree without a
/// `.git` -- e.g. the release tarball case.
fn watch_git_head() {
    let Some(git_dir) = find_git_dir() else { return };
    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // `HEAD` on a branch is just `ref: refs/heads/master`; committing moves
    // the ref file, not HEAD itself, so that has to be watched too.
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(reference) = contents.trim().strip_prefix("ref:") {
            let ref_path = git_dir.join(reference.trim());
            if ref_path.is_file() {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }
}

/// Walks up from this crate to the workspace root looking for `.git`. Handles
/// the plain-directory case; a `.git` *file* (worktrees/submodules) is
/// deliberately not followed -- it just means no rerun hint, which is
/// harmless.
fn find_git_dir() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let mut dir: Option<&Path> = Some(Path::new(&manifest_dir));
    while let Some(current) = dir {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}
