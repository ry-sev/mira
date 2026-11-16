//! Records the commit the app is built from, for the About window.
//!
//! A build outside a git checkout, or without git installed, records nothing.

use std::path::Path;
use std::process::Command;

fn main() {
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        // A new commit or checkout moves HEAD or a ref.
        let git_dir = Path::new(&git_dir);
        for watched in ["HEAD", "refs/heads", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(watched).display());
        }
    }
    if let Some(commit) = git(&["rev-parse", "--short=9", "HEAD"]) {
        println!("cargo:rustc-env=MIRA_COMMIT={commit}");
    }
}

/// What `git args` prints, trimmed, if it succeeds.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}
