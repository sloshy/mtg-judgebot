//! Two jobs.
//!
//! `sqlx::migrate!` embeds each file under `migrations/` with `include_str!`,
//! which tracks the files that exist at build time, not the directory: a
//! *new* migration would not recompile `judge-bot`, and a locally built
//! `judge-ingest migrate` (or the `#[sqlx::test]` suites) would carry a stale
//! set and report "schema is current" with one pending. This is what
//! `sqlx migrate build-script` generates; keep it beside `MIGRATOR`.
//!
//! And the build is stamped with the commit it came from, for the source
//! offer every remote interface makes (`judge_core::source`): `JUDGE_COMMIT`
//! in the build environment (the Docker build and CI set it: the image has no
//! `.git`; `JUDGE_DIRTY=1` beside it says the tree it was built from did not
//! match that commit), else `git rev-parse HEAD` in a checkout plus whether
//! the tree had uncommitted changes. The crate reads the result as
//! `JUDGE_BUILD_COMMIT` / `JUDGE_BUILD_DIRTY`; neither set means unknown,
//! which the offer says outright rather than guessing. A `JUDGE_COMMIT` that
//! is not a commit id fails the build: a branch name or a typo must not
//! silently become "commit unknown" on every interface.
//!
//! The stamp is recomputed when HEAD or the ref it points at changes (a
//! commit, a checkout) and when the variables change. The dirty flag of a
//! local build is therefore what the tree looked like at the last of those,
//! not at every edit — the image build, where it matters, always passes the
//! variables explicitly.

use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-env-changed=JUDGE_COMMIT");
    println!("cargo:rerun-if-env-changed=JUDGE_DIRTY");
    let set = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let (commit, dirty) = match set("JUDGE_COMMIT") {
        Some(v) => {
            let v = v.to_lowercase();
            let is_hash = (7..=40).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_hexdigit());
            if !is_hash {
                eprintln!(
                    "JUDGE_COMMIT={v:?} is not a git commit id (7 to 40 hex characters); \
                     pass `git rev-parse HEAD`, or leave it unset to read the checkout"
                );
                std::process::exit(1);
            }
            let dirty =
                set("JUDGE_DIRTY").is_some_and(|d| d == "1" || d.eq_ignore_ascii_case("true"));
            (Some(v), dirty)
        }
        None => from_git(),
    };
    if let Some(c) = commit {
        println!("cargo:rustc-env=JUDGE_BUILD_COMMIT={c}");
        println!("cargo:rustc-env=JUDGE_BUILD_DIRTY={}", u8::from(dirty));
    }
}

/// `git <args>`'s trimmed stdout, or `None` without git, outside a checkout
/// or on a non-zero exit.
fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// `HEAD` and whether the working tree differs from it; `(None, false)`
/// outside a checkout or without git.
fn from_git() -> (Option<String>, bool) {
    let Some(head) = git(&["rev-parse", "HEAD"]) else {
        return (None, false);
    };
    // Rerun on a new commit or checkout. The paths come from git so a
    // worktree (where `.git` is a file) resolves too, and only ones that
    // exist are declared: cargo treats a missing watched path as always
    // stale, which would rebuild this crate and its dependents on every run.
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", name]).filter(|p| Path::new(p).exists()) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    // `--porcelain` is stable output; untracked files do not make a build
    // differ from its commit, so they are left out.
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    (Some(head), dirty)
}
