//! Keeping a shallow checkout of a catalog repo (AGENTS.md rule 8: shell out to git).
//!
//! The checkout is only ever read as data: symlinks are checked out as plain files,
//! submodules are skipped, and no hooks or scripts from the repo run.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Long enough for a slow first clone; short enough not to hang startup forever.
const TIMEOUT: Duration = Duration::from_secs(90);

/// Clone `url` at `git_ref` into `dir`, or update an existing checkout. Returns the
/// checked-out commit. On failure an existing checkout is left as it was.
pub async fn sync(url: &str, git_ref: &str, dir: &Path) -> Result<String, String> {
    if dir.join(".git").is_dir() {
        git(dir, &["remote", "set-url", "origin", url]).await?;
        git(
            dir,
            &["fetch", "--quiet", "--depth", "1", "origin", git_ref],
        )
        .await?;
        git(dir, &["reset", "--quiet", "--hard", "FETCH_HEAD"]).await?;
    } else {
        clone(url, git_ref, dir).await?;
    }
    head(dir).await
}

/// Clone into a sibling temp dir and rename, so an interrupted clone never leaves a
/// half-populated checkout behind.
async fn clone(url: &str, git_ref: &str, dir: &Path) -> Result<(), String> {
    let parent = dir
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dir.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    let tmp = sibling(dir, "partial");
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    let tmp_str = tmp.to_string_lossy().to_string();
    let result = git(
        parent,
        &[
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--no-recurse-submodules",
            "--config",
            "core.symlinks=false",
            "--branch",
            git_ref,
            "--",
            url,
            &tmp_str,
        ],
    )
    .await;
    if let Err(err) = result {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err);
    }
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::rename(&tmp, dir)
        .await
        .map_err(|e| format!("moving checkout into place: {e}"))
}

pub async fn head(dir: &Path) -> Result<String, String> {
    git(dir, &["rev-parse", "HEAD"]).await
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "catalog".into());
    dir.with_file_name(format!(".{name}.{suffix}"))
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        // Never prompt for credentials: a catalog is public or it fails.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("git {} timed out", args[0]))?
        .map_err(|e| format!("running git (is it installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A local "upstream" repo with one commit.
    pub(crate) fn upstream(files: &[(&str, &str)]) -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        run(
            repo.path(),
            &["init", "--quiet", "--initial-branch", "main"],
        );
        commit(repo.path(), files);
        repo
    }

    pub(crate) fn commit(repo: &Path, files: &[(&str, &str)]) {
        for (path, body) in files {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, body).expect("write");
        }
        run(repo, &["add", "-A"]);
        run(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "-m",
                "update",
            ],
        );
    }

    pub(crate) fn run(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    pub(crate) fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    #[tokio::test]
    async fn clones_then_updates_and_keeps_cache_on_failure() {
        let up = upstream(&[("a.md", "one")]);
        let cache = tempfile::tempdir().expect("tempdir");
        let dir = cache.path().join("catalog");

        let first = sync(&file_url(up.path()), "main", &dir)
            .await
            .expect("clones");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.md")).expect("read"),
            "one"
        );

        commit(up.path(), &[("a.md", "two")]);
        let second = sync(&file_url(up.path()), "main", &dir)
            .await
            .expect("updates");
        assert_ne!(first, second);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.md")).expect("read"),
            "two"
        );

        let missing = cache.path().join("nope");
        let err = sync(&file_url(&missing), "main", &dir).await;
        assert!(err.is_err());
        assert_eq!(head(&dir).await.expect("still there"), second);
    }

    #[tokio::test]
    async fn failed_first_clone_leaves_nothing_behind() {
        let cache = tempfile::tempdir().expect("tempdir");
        let dir = cache.path().join("catalog");
        let err = sync(&file_url(&cache.path().join("nope")), "main", &dir).await;
        assert!(err.is_err());
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(cache.path()).expect("ls").count(), 0);
    }
}
