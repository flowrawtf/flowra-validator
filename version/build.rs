use std::{env, process::Command};

fn main() {
    // Watch both worktree-local and shared refs, including reftable storage.
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-dir", "--git-common-dir"])
        .output()
        && output.status.success()
        && let Ok(dirs) = String::from_utf8(output.stdout)
    {
        for dir in dirs.lines() {
            for name in ["HEAD", "refs", "packed-refs", "reftable"] {
                let path = std::path::Path::new(dir).join(name);
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }
    println!("cargo:rerun-if-env-changed=AGAVE_GIT_COMMIT_HASH");

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
