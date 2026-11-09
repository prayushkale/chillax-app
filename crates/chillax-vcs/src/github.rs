//! `VcsHost` for a project on GitHub. Local work is [`LocalGit`]'s; this adds
//! the remote: pushes through `git`, pull requests and releases through the `gh` CLI.
//! Nothing GitHub-specific lives outside this file (AGENTS.md rule 3).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chillax_core::vcs::{
    ConflictStatus, FastForward, MergeOutcome, PullRequest, RebaseResult, SyncResult, VcsError,
    VcsHost,
};
use tokio::process::Command;

use crate::{LocalGit, command, git, git_error, output, run};

pub struct GithubGit {
    local: LocalGit,
    remote: String,
    /// The `gh` program; tests point it at a script.
    gh: OsString,
}

impl GithubGit {
    /// `root` is the repository root, `base` the branch work is merged into and `remote`
    /// the git remote that points at GitHub (`[vcs] remote`).
    pub fn new(
        root: impl Into<PathBuf>,
        base: impl Into<String>,
        remote: impl Into<String>,
    ) -> Self {
        let remote = remote.into();
        Self {
            local: LocalGit::new(root, base).with_remote(remote.clone()),
            remote,
            gh: "gh".into(),
        }
    }

    /// The production branch that `dev` and `qa` are cut from.
    pub fn with_prod(mut self, prod: impl Into<String>) -> Self {
        self.local = self.local.with_prod(prod);
        self
    }

    /// See [`LocalGit::with_shared_files`].
    pub fn with_shared_files(mut self, files: Vec<String>) -> Self {
        self.local = self.local.with_shared_files(files);
        self
    }

    /// Use another program than `gh` on the PATH.
    pub fn with_gh(mut self, program: impl Into<OsString>) -> Self {
        self.gh = program.into();
        self
    }

    /// Run `gh` in the repository; its working directory tells it which repository.
    async fn gh(&self, args: &[&str]) -> Result<crate::Output, VcsError> {
        let mut cmd = Command::new(&self.gh);
        cmd.args(args)
            .current_dir(self.local.root())
            .env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let what = format!(
            "gh {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
        );
        output(&what, cmd).await
    }

    async fn gh_ok(&self, args: &[&str]) -> Result<String, VcsError> {
        let out = self.gh(args).await?;
        if out.ok {
            Ok(out.stdout)
        } else {
            let what = format!(
                "gh {}",
                args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
            );
            Err(git_error(&what, &out))
        }
    }

    /// A branch a pull request targets must exist on the remote; the first release, or a
    /// fresh repository, has no `dev` there yet.
    async fn ensure_remote_branch(&self, branch: &str) -> Result<(), VcsError> {
        let root = self.local.root();
        let heads = run(
            root,
            &["ls-remote", "--exit-code", "--heads", &self.remote, branch],
        )
        .await?;
        if heads.ok {
            return Ok(());
        }
        self.push(branch, false).await
    }

    async fn find_open_pr(&self, head: &str, base: &str) -> Result<Option<PullRequest>, VcsError> {
        let listed = self
            .gh_ok(&[
                "pr",
                "list",
                "--head",
                head,
                "--base",
                base,
                "--state",
                "open",
                "--json",
                "number,url",
            ])
            .await?;
        let prs: Vec<serde_json::Value> = serde_json::from_str(listed.trim())
            .map_err(|e| VcsError::Io(format!("unreadable `gh pr list` output: {e}")))?;
        Ok(prs.first().and_then(|pr| {
            Some(PullRequest {
                number: pr.get("number")?.as_u64()?,
                url: pr.get("url")?.as_str()?.to_string(),
            })
        }))
    }
}

#[async_trait]
impl VcsHost for GithubGit {
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), VcsError> {
        // A failed fetch (offline) only means the start point may be a little old.
        if let Err(error) = self.fetch().await {
            tracing::warn!(%error, "fetch before creating a worktree failed");
        }
        self.local.create_worktree(branch, path).await
    }

    async fn switch_branch(
        &self,
        path: &Path,
        branch: &str,
        create_from: Option<&str>,
    ) -> Result<(), VcsError> {
        self.local.switch_branch(path, branch, create_from).await
    }

    async fn remove_worktree(&self, path: &Path) -> Result<(), VcsError> {
        self.local.remove_worktree(path).await
    }

    async fn commit_all(&self, path: &Path, message: &str) -> Result<Option<String>, VcsError> {
        self.local.commit_all(path, message).await
    }

    async fn is_clean(&self, path: &Path) -> Result<bool, VcsError> {
        self.local.is_clean(path).await
    }

    async fn diff(&self, branch: &str, max_bytes: usize) -> Result<String, VcsError> {
        self.local.diff(branch, max_bytes).await
    }

    async fn merge(&self, branch: &str, message: &str) -> Result<MergeOutcome, VcsError> {
        self.local.merge(branch, message).await
    }

    async fn rebase(&self, path: &Path) -> Result<RebaseResult, VcsError> {
        if let Err(error) = self.fetch().await {
            tracing::warn!(%error, "fetch before a rebase failed");
        }
        self.local.rebase(path).await
    }

    async fn sync_with_base(&self, path: &Path, message: &str) -> Result<SyncResult, VcsError> {
        // Sync against what the pull request will merge into: the remote's base.
        if let Err(error) = self.fetch().await {
            tracing::warn!(%error, "fetch before syncing with the base failed");
        }
        self.local.sync_with_base(path, message).await
    }

    async fn conflict_status(
        &self,
        path: &Path,
        files: &[String],
    ) -> Result<ConflictStatus, VcsError> {
        self.local.conflict_status(path, files).await
    }

    async fn fast_forward(&self, branch: &str, to: &str) -> Result<FastForward, VcsError> {
        self.fetch().await?;
        self.local.fast_forward(branch, to).await
    }

    async fn merge_into(
        &self,
        target: &str,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        self.local.merge_into(target, source, message).await
    }

    async fn commit_file(
        &self,
        branch: &str,
        path: &str,
        contents: &str,
        message: &str,
    ) -> Result<String, VcsError> {
        self.local
            .commit_file(branch, path, contents, message)
            .await
    }

    async fn tag(&self, name: &str, commit: &str, message: &str) -> Result<(), VcsError> {
        self.local.tag(name, commit, message).await
    }

    async fn head(&self, branch: &str) -> Result<String, VcsError> {
        self.local.head(branch).await
    }

    async fn read_file(&self, branch: &str, path: &str) -> Result<Option<String>, VcsError> {
        self.local.read_file(branch, path).await
    }

    async fn tag_commit(&self, name: &str) -> Result<Option<String>, VcsError> {
        self.local.tag_commit(name).await
    }

    async fn checkout(&self, branch: &str) -> Result<PathBuf, VcsError> {
        self.local.checkout(branch).await
    }

    async fn detached_checkout(&self, name: &str, commit: &str) -> Result<PathBuf, VcsError> {
        self.local.detached_checkout(name, commit).await
    }

    fn uses_pull_requests(&self) -> bool {
        true
    }

    async fn push(&self, branch: &str, force_with_lease: bool) -> Result<(), VcsError> {
        let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let mut args = vec!["push", "--quiet"];
        if force_with_lease {
            args.push("--force-with-lease");
        }
        args.push(&self.remote);
        args.push(&spec);
        git(self.local.root(), &args).await?;
        Ok(())
    }

    async fn open_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<Option<PullRequest>, VcsError> {
        self.ensure_remote_branch(base).await?;
        self.fetch().await?;
        // Nothing to merge: the base already has the branch (a first release, or a retry
        // after the pull request was merged).
        let base_ref = format!("refs/remotes/{}/{base}", self.remote);
        let head_ref = format!("refs/heads/{head}");
        let merged = run(
            self.local.root(),
            &["merge-base", "--is-ancestor", &head_ref, &base_ref],
        )
        .await?;
        if merged.ok {
            return Ok(None);
        }
        if let Some(pr) = self.find_open_pr(head, base).await? {
            return Ok(Some(pr));
        }
        let created = self
            .gh_ok(&[
                "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body,
            ])
            .await?;
        let url = created
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| l.contains("/pull/"))
            .ok_or_else(|| VcsError::Io(format!("`gh pr create` printed no URL: {created}")))?
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| VcsError::Io(format!("no pull request number in {url}")))?;
        Ok(Some(PullRequest { number, url }))
    }

    async fn comment_pr(&self, number: u64, body: &str) -> Result<(), VcsError> {
        self.gh_ok(&["pr", "comment", &number.to_string(), "--body", body])
            .await?;
        Ok(())
    }

    async fn merge_pr(
        &self,
        number: u64,
        base: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        let n = number.to_string();
        let merged = self
            .gh(&["pr", "merge", &n, "--merge", "--subject", message])
            .await?;
        if !merged.ok {
            let view = self
                .gh_ok(&["pr", "view", &n, "--json", "mergeable,state"])
                .await?;
            let info: serde_json::Value = serde_json::from_str(view.trim()).unwrap_or_default();
            let state = info.get("state").and_then(|v| v.as_str()).unwrap_or("");
            let mergeable = info.get("mergeable").and_then(|v| v.as_str()).unwrap_or("");
            if state != "MERGED" {
                if mergeable == "CONFLICTING" {
                    return Ok(MergeOutcome::Conflict {
                        files: Vec::new(),
                        detail: format!(
                            "pull request #{number} conflicts with {base}:\n{}",
                            merged.stderr.trim()
                        ),
                    });
                }
                return Err(git_error("gh pr merge", &merged));
            }
        }
        self.fetch().await?;
        let refname = format!("refs/remotes/{}/{base}", self.remote);
        let commit = git(self.local.root(), &["rev-parse", "--verify", &refname]).await?;
        Ok(MergeOutcome::Merged { commit })
    }

    async fn push_tag(&self, name: &str) -> Result<(), VcsError> {
        let spec = format!("refs/tags/{name}");
        git(self.local.root(), &["push", "--quiet", &self.remote, &spec]).await?;
        Ok(())
    }

    async fn create_release(
        &self,
        tag: &str,
        title: &str,
        notes: &str,
    ) -> Result<Option<String>, VcsError> {
        // A retry finds the release it already made.
        if self.gh(&["release", "view", tag]).await?.ok {
            return Ok(None);
        }
        let created = self
            .gh_ok(&[
                "release",
                "create",
                tag,
                "--verify-tag",
                "--title",
                title,
                "--notes",
                notes,
            ])
            .await?;
        Ok(created.lines().last().map(|l| l.trim().to_string()))
    }

    async fn fetch(&self) -> Result<(), VcsError> {
        let mut cmd = command(
            self.local.root(),
            &["fetch", "--quiet", "--tags", &self.remote],
        );
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        let out = output("fetch", cmd).await?;
        if out.ok {
            Ok(())
        } else {
            Err(git_error("fetch", &out))
        }
    }

    async fn land_release(
        &self,
        prod: &str,
        base: &str,
        qa: &str,
        tag: &str,
    ) -> Result<Vec<String>, VcsError> {
        self.local.land_release(prod, base, qa, tag).await
    }

    async fn sync_local(&self, branch: &str) -> Result<Option<String>, VcsError> {
        self.local.sync_branch(branch).await
    }

    async fn remote_head(&self, branch: &str) -> Result<Option<String>, VcsError> {
        self.local.remote_ref(branch).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_GH: &str = include_str!("../testdata/fake-gh.sh");

    struct Fixture {
        work: tempfile::TempDir,
        remote: tempfile::TempDir,
        bin: tempfile::TempDir,
        git: GithubGit,
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A work repo on `main` with `dev`, both pushed to a bare remote, and a fake `gh`.
    fn fixture() -> Fixture {
        let work = tempfile::tempdir().expect("tempdir");
        let remote = tempfile::tempdir().expect("tempdir");
        let bin = tempfile::tempdir().expect("tempdir");
        sh(remote.path(), &["init", "--quiet", "--bare", "-b", "main"]);
        sh(work.path(), &["init", "--quiet", "-b", "main"]);
        std::fs::write(work.path().join("README.md"), "hello\n").expect("write");
        std::fs::write(work.path().join(".gitignore"), ".chillax/worktrees/\n").expect("write");
        sh(work.path(), &["add", "-A"]);
        sh(work.path(), &["commit", "--quiet", "-m", "initial"]);
        sh(work.path(), &["branch", "dev"]);
        let url = remote.path().to_string_lossy().to_string();
        sh(work.path(), &["remote", "add", "origin", &url]);
        sh(work.path(), &["push", "--quiet", "origin", "main", "dev"]);
        let script = bin.path().join("gh");
        std::fs::write(&script, FAKE_GH).expect("write gh");
        std::fs::write(bin.path().join("remote"), &url).expect("write remote");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let git = GithubGit::new(work.path(), "dev", "origin").with_gh(script);
        Fixture {
            work,
            remote,
            bin,
            git,
        }
    }

    impl Fixture {
        fn gh_log(&self) -> String {
            std::fs::read_to_string(self.bin.path().join("gh.log")).unwrap_or_default()
        }

        fn remote_head(&self, branch: &str) -> String {
            sh(self.remote.path(), &["rev-parse", branch])
        }

        fn local_head(&self, branch: &str) -> String {
            sh(self.work.path(), &["rev-parse", branch])
        }

        /// A ticket branch with one commit of `file`, pushed.
        async fn ticket(&self, name: &str, file: &str, body: &str) -> String {
            let branch = format!("feat/{name}");
            let path = self.work.path().join(".chillax/worktrees").join(name);
            self.git
                .create_worktree(&branch, &path)
                .await
                .expect("worktree");
            std::fs::write(path.join(file), body).expect("write");
            self.git
                .commit_all(&path, &format!("{name}: {file}"))
                .await
                .expect("commit")
                .expect("changes");
            self.git.push(&branch, true).await.expect("push");
            branch
        }
    }

    #[tokio::test]
    async fn a_ticket_branch_goes_through_a_pull_request_into_dev() {
        let f = fixture();
        let branch = f.ticket("T-1", "a.txt", "a\n").await;
        let pr = f
            .git
            .open_pr(&branch, "dev", "T-1: add a", "body")
            .await
            .expect("open")
            .expect("a pull request");
        assert_eq!(pr.number, 1);
        assert_eq!(pr.url, "https://github.com/o/r/pull/1");
        // Asking again, as after a rework, finds the same one.
        let again = f
            .git
            .open_pr(&branch, "dev", "T-1: add a", "body")
            .await
            .expect("open again")
            .expect("a pull request");
        assert_eq!(again, pr);
        assert_eq!(f.gh_log().matches("pr create").count(), 1);
        assert!(f.gh_log().contains("pr create --base dev --head feat/T-1"));

        f.git.comment_pr(1, "approved").await.expect("comment");
        assert!(f.gh_log().contains("pr comment 1 --body approved"));

        let before = f.remote_head("dev");
        let merged = f.git.merge_pr(1, "dev", "Merge T-1").await.expect("merge");
        let MergeOutcome::Merged { commit } = merged else {
            panic!("expected a merge, got {merged:?}");
        };
        assert_eq!(commit, f.remote_head("dev"));
        assert_ne!(commit, before);
        assert!(
            f.gh_log()
                .contains("pr merge 1 --merge --subject Merge T-1")
        );

        // The local dev is not checked out anywhere, so it simply follows the remote.
        assert_eq!(f.git.sync_local("dev").await.expect("sync"), None);
        assert_eq!(f.local_head("dev"), commit);
    }

    #[tokio::test]
    async fn a_dirty_checkout_is_left_alone_and_said_so() {
        let f = fixture();
        let branch = f.ticket("T-1", "a.txt", "a\n").await;
        f.git.open_pr(&branch, "dev", "t", "b").await.expect("open");
        f.git.merge_pr(1, "dev", "Merge").await.expect("merge");
        let old = f.local_head("dev");
        // The stakeholder works on dev in the repository itself.
        sh(f.work.path(), &["switch", "--quiet", "dev"]);
        std::fs::write(f.work.path().join("README.md"), "edited\n").expect("write");
        let note = f
            .git
            .sync_local("dev")
            .await
            .expect("sync")
            .expect("a note");
        assert!(note.contains("local dev is behind origin/dev"), "{note}");
        assert_eq!(f.local_head("dev"), old, "nothing moved");
        // Once the checkout is clean the next sync catches up.
        sh(f.work.path(), &["checkout", "--quiet", "--", "README.md"]);
        assert_eq!(f.git.sync_local("dev").await.expect("sync"), None);
        assert_eq!(f.local_head("dev"), f.remote_head("dev"));
    }

    #[tokio::test]
    async fn a_conflicting_pull_request_is_a_conflict_not_an_error() {
        let f = fixture();
        let a = f.ticket("T-1", "same.txt", "one\n").await;
        let b = f.ticket("T-2", "same.txt", "two\n").await;
        f.git.open_pr(&a, "dev", "a", "b").await.expect("open a");
        f.git.open_pr(&b, "dev", "b", "b").await.expect("open b");
        f.git
            .merge_pr(1, "dev", "Merge T-1")
            .await
            .expect("merge a");
        let second = f
            .git
            .merge_pr(2, "dev", "Merge T-2")
            .await
            .expect("merge b");
        assert!(
            matches!(second, MergeOutcome::Conflict { .. }),
            "got {second:?}"
        );
    }

    #[tokio::test]
    async fn a_branch_the_base_already_has_opens_no_pull_request() {
        let f = fixture();
        sh(f.work.path(), &["branch", "feat/empty", "dev"]);
        let none = f
            .git
            .open_pr("feat/empty", "dev", "t", "b")
            .await
            .expect("open");
        assert_eq!(none, None);
        assert_eq!(f.gh_log().matches("pr create").count(), 0);
    }

    #[tokio::test]
    async fn a_missing_base_branch_is_pushed_before_the_pull_request() {
        let f = fixture();
        sh(f.work.path(), &["branch", "qa", "dev"]);
        std::fs::write(f.work.path().join("x.txt"), "x\n").expect("write");
        sh(f.work.path(), &["switch", "--quiet", "-c", "feat/x", "dev"]);
        sh(f.work.path(), &["add", "-A"]);
        sh(f.work.path(), &["commit", "--quiet", "-m", "x"]);
        f.git.push("feat/x", true).await.expect("push");
        // `qa` is not on the remote yet.
        let pr = f.git.open_pr("feat/x", "qa", "t", "b").await.expect("open");
        assert!(pr.is_some());
        assert_eq!(f.remote_head("qa"), f.local_head("dev"));
    }

    #[tokio::test]
    async fn tags_and_releases_are_published_once() {
        let f = fixture();
        let dev = f.local_head("dev");
        f.git.tag("v0.1.0", &dev, "notes").await.expect("tag");
        f.git.push_tag("v0.1.0").await.expect("push tag");
        f.git.push_tag("v0.1.0").await.expect("push tag again");
        assert!(sh(f.remote.path(), &["tag", "-l"]).contains("v0.1.0"));
        let url = f
            .git
            .create_release("v0.1.0", "v0.1.0", "the notes")
            .await
            .expect("release");
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/o/r/releases/tag/v0.1.0")
        );
        assert_eq!(
            f.git
                .create_release("v0.1.0", "v0.1.0", "the notes")
                .await
                .expect("again"),
            None
        );
        assert_eq!(f.gh_log().matches("release create").count(), 1);
    }

    #[tokio::test]
    async fn new_work_starts_from_the_remote_dev_not_a_stale_local_one() {
        let f = fixture();
        let branch = f.ticket("T-1", "a.txt", "a\n").await;
        f.git.open_pr(&branch, "dev", "t", "b").await.expect("open");
        f.git.merge_pr(1, "dev", "Merge").await.expect("merge");
        // The local dev still lacks the merge; a new ticket must not start without it.
        let path = f.work.path().join(".chillax/worktrees/T-2");
        f.git.create_worktree("feat/T-2", &path).await.expect("wt");
        assert!(path.join("a.txt").exists());
    }

    #[tokio::test]
    async fn promotion_follows_the_remote_dev() {
        let f = fixture();
        let branch = f.ticket("T-1", "a.txt", "a\n").await;
        f.git.open_pr(&branch, "dev", "t", "b").await.expect("open");
        f.git.merge_pr(1, "dev", "Merge").await.expect("merge");
        let moved = f.git.fast_forward("qa", "dev").await.expect("ff");
        assert_eq!(
            moved,
            FastForward::Advanced {
                commit: f.remote_head("dev")
            }
        );
    }
}
