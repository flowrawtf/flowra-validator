use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Without this the build script is cached against the package's own files, so `src:` in
    // --version keeps reporting whichever commit was checked out the first time the crate was
    // built. Rerun when HEAD moves, and when the ref HEAD points at moves.
    println!("cargo:rerun-if-env-changed=AGAVE_GIT_COMMIT_HASH");
    for path in ["HEAD", "@"] {
        if let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", path])
            .output()
            && output.status.success()
            && let Ok(path) = String::from_utf8(output.stdout)
        {
            println!("cargo:rerun-if-changed={}", path.trim());
        }
    }

    // A build from a release tarball has no git history, so let the caller supply the hash the
    // tarball was cut from. That is what makes the published binary reproducible off-tree.
    if let Ok(hash) = env::var("AGAVE_GIT_COMMIT_HASH") {
        let trimmed_hash = hash.trim();
        if !trimmed_hash.is_empty() {
            println!("cargo:rustc-env=AGAVE_GIT_COMMIT_HASH={trimmed_hash}");
            return;
        }
    }

    if let Ok(git_output) = Command::new("git").args(["rev-parse", "HEAD"]).output()
        && git_output.status.success()
        && let Ok(git_commit_hash) = String::from_utf8(git_output.stdout)
    {
        let trimmed_hash = git_commit_hash.trim().to_string();
        println!("cargo:rustc-env=AGAVE_GIT_COMMIT_HASH={trimmed_hash}");
    }
}
