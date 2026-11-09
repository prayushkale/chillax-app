//! `VcsHost` over the `git` binary (AGENTS.md rule 8): merges into the base, `qa` and prod
//! branches are made in one worktree per branch (`_dev`, `_qa`, `_main`), so the
//! stakeholder's own checkout is never touched. Ticket work gets its own worktree only in
//! `per-ticket` mode; by default it happens on a branch in `_dev` (ADR 0025).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chillax_core::vcs::{
    ConflictStatus, FastForward, MergeOutcome, RebaseResult, SyncResult, VcsError, VcsHost,
};
use tokio::process::Command;

mod github;

pub use github::GithubGit;

const TIMEOUT: Duration = Duration::from_secs(120);

/// Where merges into the default base branch happen, relative to the repository root.
/// Other branches get `_<branch>` next to it.
pub const INTEGRATION_WORKTREE: &str = ".chillax/worktrees/_dev";

pub struct LocalGit {
    root: PathBuf,
    base: String,
    /// The git remote whose branches are the truth for the base branch, when work is
    /// shared through one. `None` keeps everything local.
    remote: Option<String>,
    /// The production branch, which `dev` and `qa` are cut from. `None` means `main`.
    prod: Option<String>,
    /// Gitignored files in the root that every worktree links to (`[env] files`).
    shared_files: Vec<String>,
}

pub(crate) struct Output {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl LocalGit {
    /// `root` is the repository root; `base` the branch work is cut from and merged
    /// into (`[env.dev] base_branch`).
    pub fn new(root: impl Into<PathBuf>, base: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            base: base.into(),
            remote: None,
            prod: None,
            shared_files: Vec::new(),
        }
    }

    /// Files in the repository root (`.env` and the like) that git does not track but
    /// every worktree should see.
    pub fn with_shared_files(mut self, files: Vec<String>) -> Self {
        self.shared_files = files;
        self
    }

    /// Link the shared files into a worktree Chillax manages. Failing to is logged, not
    /// fatal: the work can go on, it just lacks the file.
    async fn share_files(&self, dir: &Path) {
        if let Err(error) = share_files(&self.root, dir, &self.shared_files).await {
            tracing::warn!(%error, dir = %dir.display(), "could not link the env files");
        }
    }

    /// The production branch (`[env.prod] branch`) that `dev` and `qa` are cut from.
    pub fn with_prod(mut self, prod: impl Into<String>) -> Self {
        self.prod = Some(prod.into());
        self
    }

    fn prod(&self) -> &str {
        self.prod.as_deref().unwrap_or("main")
    }

    /// Make sure the base branch and every branch in `extra` (for example `qa`) exist,
    /// cut from the production branch, so a worktree never fails on a missing `dev`.
    /// A repository with no commits gets an empty first commit on the production branch.
    /// Existing branches are never moved, and the production branch is never created
    /// except by that first commit. Returns the names it created.
    pub async fn ensure_branches(&self, extra: &[&str]) -> Result<Vec<String>, VcsError> {
        let mut created = Vec::new();
        if !run(&self.root, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .await?
            .ok
        {
            let prod_ref = format!("refs/heads/{}", self.prod());
            git(&self.root, &["symbolic-ref", "HEAD", &prod_ref]).await?;
            let mut cmd = command(
                &self.root,
                &["commit", "--allow-empty", "-q", "-m", "Initial commit"],
            );
            if git(&self.root, &["config", "user.email"]).await.is_err() {
                cmd.env("GIT_AUTHOR_NAME", "Chillax")
                    .env("GIT_AUTHOR_EMAIL", "chillax@localhost");
            }
            let made = output("commit", cmd).await?;
            if !made.ok {
                return Err(git_error("commit", &made));
            }
            created.push(self.prod().to_string());
        }
        let prod = self.prod().to_string();
        let source = if self.branch_exists(&prod).await? {
            git(&self.root, &["rev-parse", &format!("refs/heads/{prod}")]).await?
        } else if let Some(sha) = self.remote_ref(&prod).await? {
            sha
        } else {
            git(&self.root, &["rev-parse", "HEAD"]).await?
        };
        let mut wanted: Vec<&str> = vec![self.base.as_str()];
        wanted.extend(extra.iter().copied());
        let mut seen = Vec::new();
        for name in wanted {
            if name == prod || seen.contains(&name) {
                continue;
            }
            seen.push(name);
            if self.branch_exists(name).await? {
                continue;
            }
            let start = self.remote_ref(name).await?.unwrap_or(source.clone());
            let made = run(&self.root, &["branch", name, &start]).await?;
            if made.ok {
                created.push(name.to_string());
            } else if !self.branch_exists(name).await? {
                return Err(git_error("branch", &made));
            }
        }
        Ok(created)
    }

    /// Start new branches from, diff against, rebase onto and promote from the remote's
    /// copy of the base branch (when it has one), so a stale local `dev` is never used.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The remote-tracking ref for `branch` if there is a remote and it has the branch.
    async fn remote_ref(&self, branch: &str) -> Result<Option<String>, VcsError> {
        let Some(remote) = &self.remote else {
            return Ok(None);
        };
        let refname = format!("refs/remotes/{remote}/{branch}");
        let found = run(&self.root, &["rev-parse", "--verify", "--quiet", &refname]).await?;
        Ok(found.ok.then(|| found.stdout.trim().to_string()))
    }

    /// What new work is cut from and compared with: the remote's base, else the local one.
    async fn base_ref(&self) -> Result<String, VcsError> {
        match self.remote_ref(&self.base).await? {
            Some(sha) => Ok(sha),
            None => Ok(self.base.clone()),
        }
    }

    /// Fetch, then bring the local `branch` up to the remote's where nobody is disturbed.
    pub(crate) async fn sync_branch(&self, branch: &str) -> Result<Option<String>, VcsError> {
        let Some(remote) = self.remote.clone() else {
            return Ok(None);
        };
        git(&self.root, &["fetch", "--quiet", "--tags", &remote]).await?;
        let Some(target) = self.remote_ref(branch).await? else {
            return Ok(None);
        };
        if !self.branch_exists(branch).await? {
            git(&self.root, &["branch", branch, &target]).await?;
            return Ok(None);
        }
        let local = self.head(branch).await?;
        if local == target || self.is_ancestor(&target, &local).await? {
            return Ok(None);
        }
        if !self.is_ancestor(&local, &target).await? {
            return Ok(Some(format!(
                "local {branch} has commits that {remote}/{branch} lacks and the other way                  round; Chillax left it alone"
            )));
        }
        match self.checkout_of(branch).await? {
            None => {
                let refname = format!("refs/heads/{branch}");
                git(&self.root, &["update-ref", &refname, &target, &local]).await?;
                Ok(None)
            }
            Some(dir) if self.is_clean(&dir).await? => {
                git(&dir, &["merge", "--ff-only", "--quiet", &target]).await?;
                Ok(None)
            }
            Some(dir) => Ok(Some(format!(
                "local {branch} is behind {remote}/{branch}: {} has uncommitted changes, so                  Chillax left it. Commit or stash them and pull when convenient",
                dir.display()
            ))),
        }
    }

    /// After a release is deployed, bring `base` (dev), `qa` and `prod` (main) to the same
    /// commit where that is a fast-forward, push them and the tag, and put the primary
    /// checkout on `prod`. Never forces; whatever cannot be done safely is skipped and
    /// described in the returned notes. Doing it twice is harmless.
    pub(crate) async fn land_release(
        &self,
        prod: &str,
        base: &str,
        qa: &str,
        tag: &str,
    ) -> Result<Vec<String>, VcsError> {
        let mut notes = Vec::new();
        // The remote's main, when it is ahead, becomes the local one first.
        match self.sync_branch(prod).await {
            Ok(Some(note)) => notes.push(note),
            Ok(None) => {}
            Err(e) => notes.push(format!("could not fetch before syncing: {e}")),
        }
        if !self.branch_exists(prod).await? {
            notes.push(format!("there is no `{prod}` branch to converge on"));
            return Ok(notes);
        }
        let main = self.head(prod).await?;

        // qa follows main.
        match self.fast_forward(qa, prod).await {
            Ok(FastForward::Diverged { detail }) => notes.push(detail),
            Ok(_) => {}
            Err(e) => notes.push(format!("could not move `{qa}` to `{prod}`: {e}")),
        }

        // dev follows main only when it is behind it. Ahead of main (it holds main already)
        // is normal: work has merged since the release. Neither is a divergence to report.
        if !self.branch_exists(base).await? {
            git(&self.root, &["branch", base, &main]).await?;
        } else {
            let dev = self.head(base).await?;
            if dev != main && !self.is_ancestor(&main, &dev).await? {
                if self.is_ancestor(&dev, &main).await? {
                    if let Some(note) = self.ff_local(base, &main).await? {
                        notes.push(note);
                    }
                } else {
                    notes.push(format!(
                        "`{base}` has commits that `{prod}` lacks and the other way round, so \
                         Chillax left `{base}` alone; merge `{prod}` into it"
                    ));
                }
            }
        }

        if let Some(remote) = self.remote.clone() {
            for branch in [base, qa, prod] {
                if !self.branch_exists(branch).await? {
                    continue;
                }
                let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
                if let Err(e) = git(&self.root, &["push", "--quiet", &remote, &spec]).await {
                    notes.push(format!("could not push `{branch}` to {remote}: {e}"));
                }
            }
            let spec = format!("refs/tags/{tag}");
            if let Err(e) = git(&self.root, &["push", "--quiet", &remote, &spec]).await {
                notes.push(format!("could not push tag {tag} to {remote}: {e}"));
            }
        }

        if let Some(note) = self.switch_primary_to(prod).await? {
            notes.push(note);
        }
        Ok(notes)
    }

    /// Fast-forward the local `branch` to `target` (a descendant) where nobody is disturbed.
    async fn ff_local(&self, branch: &str, target: &str) -> Result<Option<String>, VcsError> {
        let local = self.head(branch).await?;
        match self.checkout_of(branch).await? {
            None => {
                let refname = format!("refs/heads/{branch}");
                git(&self.root, &["update-ref", &refname, target, &local]).await?;
                Ok(None)
            }
            Some(dir) if self.is_clean(&dir).await? => {
                let moved = run(&dir, &["merge", "--ff-only", "--quiet", target]).await?;
                Ok((!moved.ok)
                    .then(|| format!("could not fast-forward `{branch}`: {}", moved.stderr.trim())))
            }
            Some(dir) => Ok(Some(format!(
                "`{branch}` is behind but {} has uncommitted changes, so Chillax left it",
                dir.display()
            ))),
        }
    }

    /// Check `prod` out in the primary checkout, only when that loses nothing and
    /// disturbs nobody. `None` when it is there afterwards.
    async fn switch_primary_to(&self, prod: &str) -> Result<Option<String>, VcsError> {
        let current = current_branch(&self.root).await?;
        if current.as_deref() == Some(prod) {
            return Ok(None);
        }
        let skip = |why: String| Ok(Some(format!("left the main checkout alone: {why}")));
        if !self.is_clean(&self.root).await? {
            return skip(format!(
                "it has uncommitted changes{}",
                self.dirty_note(&self.root).await
            ));
        }
        let git_dir = git(&self.root, &["rev-parse", "--absolute-git-dir"]).await?;
        for marker in [
            "MERGE_HEAD",
            "rebase-merge",
            "rebase-apply",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
        ] {
            if Path::new(&git_dir).join(marker).exists() {
                return skip(format!(
                    "a merge, rebase or similar is in progress ({marker})"
                ));
            }
        }
        if let Some(other) = self.checkout_of(prod).await? {
            return skip(format!("`{prod}` is checked out at {}", other.display()));
        }
        if current.is_none() {
            let held = git(&self.root, &["branch", "--contains", "HEAD"]).await?;
            if held.is_empty() {
                return skip("it is on a detached commit no branch holds".into());
            }
        }
        let switched = run(&self.root, &["switch", "--quiet", prod]).await?;
        if switched.ok {
            Ok(None)
        } else {
            skip(format!("git switch failed: {}", switched.stderr.trim()))
        }
    }

    async fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool, VcsError> {
        Ok(
            run(&self.root, &["merge-base", "--is-ancestor", ancestor, of])
                .await?
                .ok,
        )
    }

    /// The first five paths `git status --porcelain` reports in `dir`, so a "dirty checkout"
    /// message can name what to commit or stash. Does not change what counts as dirty.
    pub(crate) async fn dirty_paths(&self, dir: &Path) -> Result<Vec<String>, VcsError> {
        let out = git_raw(dir, &["status", "--porcelain"]).await?;
        Ok(out
            .lines()
            .filter_map(|l| l.get(3..))
            .take(5)
            .map(str::to_string)
            .collect())
    }

    /// ` (a, b, ...)` for the message, or nothing when git lists no paths.
    async fn dirty_note(&self, dir: &Path) -> String {
        match self.dirty_paths(dir).await {
            Ok(paths) if !paths.is_empty() => format!(" ({})", paths.join(", ")),
            _ => String::new(),
        }
    }

    /// Write `contents` to `path` in `dir` and commit it; the commit hash, or `dir`'s HEAD
    /// when nothing changed.
    async fn commit_file_in(
        &self,
        dir: &Path,
        path: &str,
        contents: &str,
        message: &str,
    ) -> Result<String, VcsError> {
        let relative = Path::new(path);
        let file = dir.join(relative);
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VcsError::Io(e.to_string()))?;
        }
        tokio::fs::write(&file, contents)
            .await
            .map_err(|e| VcsError::Io(e.to_string()))?;
        git(dir, &["add", "--", path]).await?;
        if git(dir, &["status", "--porcelain"]).await?.is_empty() {
            return git(dir, &["rev-parse", "HEAD"]).await;
        }
        let mut cmd = command(dir, &["commit", "--quiet", "-m", message]);
        if git(dir, &["config", "user.email"]).await.is_err() {
            cmd.env("GIT_AUTHOR_NAME", "Chillax")
                .env("GIT_AUTHOR_EMAIL", "chillax@localhost");
        }
        let committed = output("commit", cmd).await?;
        if !committed.ok {
            return Err(git_error("commit", &committed));
        }
        git(dir, &["rev-parse", "HEAD"]).await
    }

    /// A checkout of `branch` that is not Chillax's own integration worktree and holds
    /// uncommitted changes, usually the user's own working tree. Chillax never needs it
    /// clean: merges are built elsewhere and fast-forwarded into it (`merge_beside`).
    async fn dirty_holder(&self, branch: &str) -> Result<Option<PathBuf>, VcsError> {
        if !self.branch_exists(branch).await? {
            return Ok(None);
        }
        let own = self.integration_path_of(branch);
        match self.checkout_of(branch).await? {
            Some(dir) if dir != own && !self.is_clean(&dir).await? => Ok(Some(dir)),
            _ => Ok(None),
        }
    }

    /// Merge `source` into `target` while `target` is checked out in `holder` with
    /// uncommitted changes. The merge is built in a throwaway detached worktree, then
    /// `holder` is fast-forwarded to it with `git merge --ff-only`, which leaves unrelated
    /// local changes alone and refuses (touching nothing) when they overlap the merge.
    async fn merge_beside(
        &self,
        holder: &Path,
        target: &str,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        let scratch = self.scratch_worktree(target).await?;
        let built = self.merge_in(&scratch, source, message).await;
        self.drop_scratch(&scratch).await;
        let commit = match built? {
            MergeOutcome::Merged { commit } => commit,
            conflict => return Ok(conflict),
        };
        self.ff_holder(holder, target, &commit).await?;
        Ok(MergeOutcome::Merged { commit })
    }

    /// A detached worktree at `target`, for building a commit away from a dirty checkout.
    async fn scratch_worktree(&self, target: &str) -> Result<PathBuf, VcsError> {
        let scratch = self.root.join(".chillax/worktrees").join(format!(
            "_merge-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        if let Some(parent) = scratch.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VcsError::Io(e.to_string()))?;
        }
        let path = scratch.to_string_lossy().to_string();
        git(&self.root, &["worktree", "add", "--detach", &path, target]).await?;
        Ok(scratch)
    }

    async fn drop_scratch(&self, scratch: &Path) {
        let path = scratch.to_string_lossy().to_string();
        let _ = run(&self.root, &["worktree", "remove", "--force", &path]).await;
    }

    /// Fast-forward the dirty checkout `holder` to `commit`; on overlap git changes
    /// nothing and this returns `BaseBusy`.
    async fn ff_holder(&self, holder: &Path, target: &str, commit: &str) -> Result<(), VcsError> {
        let moved = run(holder, &["merge", "--ff-only", "--quiet", commit]).await?;
        if moved.ok {
            return Ok(());
        }
        Err(VcsError::BaseBusy(format!(
            "{target} is checked out at {} with uncommitted changes{} that overlap what \
             Chillax is committing; commit or stash them (or switch that checkout to another \
             branch) so Chillax can proceed. Nothing was changed there",
            holder.display(),
            self.dirty_note(holder).await
        )))
    }

    /// Problems that would make a local release fail, found before it starts.
    pub(crate) async fn preflight(
        &self,
        qa: &str,
        prod: &str,
        base: &str,
    ) -> Result<Vec<String>, VcsError> {
        let mut problems = Vec::new();
        if self.branch_exists(prod).await? && self.branch_exists(qa).await? {
            let head = self.head(prod).await?;
            if !self.is_ancestor(&head, qa).await? {
                problems.push(format!(
                    "`{prod}` has commits that `{qa}` lacks (someone committed to `{prod}` \
                     directly). Merge `{prod}` into `{base}` (git switch {base} && git merge \
                     {prod}) so `{qa}` can catch up, then approve the release again"
                ));
            }
        }
        Ok(problems)
    }

    /// Where the base branch's integration worktree lives, for callers that want to
    /// show it.
    pub fn integration_path(&self) -> PathBuf {
        self.integration_path_of(&self.base)
    }

    fn integration_path_of(&self, branch: &str) -> PathBuf {
        if branch == "dev" {
            return self.root.join(INTEGRATION_WORKTREE);
        }
        let name: String = branch
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.root
            .join(".chillax/worktrees")
            .join(format!("_{name}"))
    }

    /// The directory to merge `branch` in: its integration worktree, or the place it is
    /// already checked out when git will not allow a second checkout.
    async fn integration_of(&self, branch: &str) -> Result<PathBuf, VcsError> {
        let dir = self.integration_path_of(branch);
        if dir.join(".git").exists() {
            // A crashed merge must not poison the next one.
            let _ = run(&dir, &["merge", "--abort"]).await;
            git(&dir, &["reset", "--hard", "--quiet"]).await?;
            // A shared worktree is moved between branches (ADR 0025): merges need it on
            // `branch`. If git refuses because another checkout holds `branch`, merge there.
            if current_branch(&dir).await?.as_deref() != Some(branch)
                && !run(&dir, &["switch", "--quiet", branch]).await?.ok
            {
                return match self.checkout_of(branch).await? {
                    Some(existing) if existing != dir => {
                        if self.is_clean(&existing).await? {
                            Ok(existing)
                        } else {
                            Err(VcsError::BaseBusy(format!(
                                "{branch} is checked out at {} with uncommitted changes{}; \
                                 commit or stash them so Chillax can merge",
                                existing.display(),
                                self.dirty_note(&existing).await
                            )))
                        }
                    }
                    _ => Err(VcsError::Git {
                        command: "switch".into(),
                        detail: format!("could not check out `{branch}` in {}", dir.display()),
                    }),
                };
            }
            self.share_files(&dir).await;
            return Ok(dir);
        }
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VcsError::Io(e.to_string()))?;
        }
        let path = dir.to_string_lossy().to_string();
        let added = run(&self.root, &["worktree", "add", &path, branch]).await?;
        if added.ok {
            self.share_files(&dir).await;
            return Ok(dir);
        }
        // The branch is checked out in another worktree, usually the repository itself.
        let Some(existing) = self.checkout_of(branch).await? else {
            return Err(git_error("worktree add", &added));
        };
        if !self.is_clean(&existing).await? {
            return Err(VcsError::BaseBusy(format!(
                "{branch} is checked out at {} with uncommitted changes{}; commit or stash them \
                 so Chillax can merge",
                existing.display(),
                self.dirty_note(&existing).await
            )));
        }
        Ok(existing)
    }

    /// Merge `source` into `target` in `target`'s integration worktree.
    async fn merge_branch(
        &self,
        target: &str,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        if let Some(holder) = self.dirty_holder(target).await? {
            return self.merge_beside(&holder, target, source, message).await;
        }
        let dir = self.integration_of(target).await?;
        self.merge_in(&dir, source, message).await
    }

    /// Merge `source` into whatever `dir` has checked out; a conflict is aborted.
    async fn merge_in(
        &self,
        dir: &Path,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        let dir = dir.to_path_buf();
        let merged = run(&dir, &["merge", "--no-ff", "-m", message, source]).await?;
        if merged.ok {
            let commit = git(&dir, &["rev-parse", "HEAD"]).await?;
            return Ok(MergeOutcome::Merged { commit });
        }
        let in_progress = run(&dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .await?
            .ok;
        if !in_progress {
            return Err(git_error("merge", &merged));
        }
        let files = unmerged_paths(&dir).await?;
        git(&dir, &["merge", "--abort"]).await?;
        Ok(MergeOutcome::Conflict {
            detail: conflict_detail(&files),
            files,
        })
    }

    /// The worktree that has `branch` checked out, if any.
    async fn checkout_of(&self, branch: &str) -> Result<Option<PathBuf>, VcsError> {
        let listing = git(&self.root, &["worktree", "list", "--porcelain"]).await?;
        let want = format!("branch refs/heads/{branch}");
        let mut path = None;
        for line in listing.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line == want {
                return Ok(path);
            }
        }
        Ok(None)
    }

    async fn branch_exists(&self, branch: &str) -> Result<bool, VcsError> {
        let refname = format!("refs/heads/{branch}");
        Ok(
            run(&self.root, &["rev-parse", "--verify", "--quiet", &refname])
                .await?
                .ok,
        )
    }
}

#[async_trait]
impl VcsHost for LocalGit {
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), VcsError> {
        if path.join(".git").exists() {
            self.share_files(path).await;
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VcsError::Io(e.to_string()))?;
        }
        let dir = path.to_string_lossy().to_string();
        if self.branch_exists(branch).await? {
            git(&self.root, &["worktree", "add", &dir, branch]).await?;
        } else {
            // A repo that had no commits when it was set up never got its `dev` branch.
            if self.remote_ref(&self.base).await?.is_none()
                && !self.branch_exists(&self.base).await?
            {
                self.ensure_branches(&[]).await?;
            }
            let start = self.base_ref().await?;
            git(&self.root, &["worktree", "add", "-b", branch, &dir, &start]).await?;
        }
        self.share_files(path).await;
        Ok(())
    }

    async fn switch_branch(
        &self,
        path: &Path,
        branch: &str,
        create_from: Option<&str>,
    ) -> Result<(), VcsError> {
        if current_branch(path).await?.as_deref() == Some(branch) {
            return Ok(());
        }
        if self.branch_exists(branch).await? {
            let switched = run(path, &["switch", "--quiet", branch]).await?;
            if switched.ok {
                return Ok(());
            }
            // The base branch can be checked out in the stakeholder's own repository, and
            // git allows one checkout of a branch. Park on its commit instead; merges then
            // happen in that checkout (`integration_of`).
            if branch == self.base && self.checkout_of(branch).await?.is_some() {
                git(path, &["switch", "--quiet", "--detach", branch]).await?;
                return Ok(());
            }
            return Err(git_error("switch", &switched));
        }
        let from = create_from.ok_or_else(|| VcsError::Git {
            command: "switch".into(),
            detail: format!("there is no branch `{branch}` and nothing to cut it from"),
        })?;
        let start = if from == self.base {
            self.base_ref().await?
        } else {
            from.to_string()
        };
        git(path, &["switch", "--quiet", "-c", branch, &start]).await?;
        Ok(())
    }

    async fn remove_worktree(&self, path: &Path) -> Result<(), VcsError> {
        if path.exists() {
            let dir = path.to_string_lossy().to_string();
            git(&self.root, &["worktree", "remove", "--force", &dir]).await?;
        } else {
            git(&self.root, &["worktree", "prune"]).await?;
        }
        Ok(())
    }

    async fn commit_all(&self, path: &Path, message: &str) -> Result<Option<String>, VcsError> {
        git(path, &["add", "-A"]).await?;
        if git(path, &["status", "--porcelain"]).await?.is_empty() {
            return Ok(None);
        }
        let mut cmd = command(path, &["commit", "--quiet", "-m", message]);
        // A machine without a git identity still gets its work committed.
        if git(path, &["config", "user.email"]).await.is_err() {
            cmd.env("GIT_AUTHOR_NAME", "Chillax")
                .env("GIT_AUTHOR_EMAIL", "chillax@localhost");
        }
        let committed = output("commit", cmd).await?;
        if !committed.ok {
            return Err(git_error("commit", &committed));
        }
        Ok(Some(git(path, &["rev-parse", "HEAD"]).await?))
    }

    async fn is_clean(&self, path: &Path) -> Result<bool, VcsError> {
        Ok(git(path, &["status", "--porcelain"]).await?.is_empty())
    }

    async fn diff(&self, branch: &str, max_bytes: usize) -> Result<String, VcsError> {
        let range = format!("{}...{branch}", self.base_ref().await?);
        let mut diff = git_raw(&self.root, &["diff", &range]).await?;
        if diff.len() > max_bytes {
            let mut cut = max_bytes;
            while !diff.is_char_boundary(cut) {
                cut -= 1;
            }
            diff.truncate(cut);
            diff.push_str("\n… (diff truncated)\n");
        }
        Ok(diff)
    }

    async fn merge(&self, branch: &str, message: &str) -> Result<MergeOutcome, VcsError> {
        self.merge_branch(&self.base, branch, message).await
    }

    async fn rebase(&self, path: &Path) -> Result<RebaseResult, VcsError> {
        if !self.is_clean(path).await? {
            return Ok(RebaseResult::Dirty);
        }
        let onto = self.base_ref().await?;
        let rebased = run(path, &["rebase", &onto]).await?;
        if rebased.ok {
            return Ok(RebaseResult::Rebased);
        }
        let files = unmerged_paths(path).await?;
        git(path, &["rebase", "--abort"]).await?;
        Ok(RebaseResult::Conflict {
            detail: conflict_detail(&files),
            files,
        })
    }

    async fn sync_with_base(&self, path: &Path, message: &str) -> Result<SyncResult, VcsError> {
        if !self.is_clean(path).await? {
            return Ok(SyncResult::Dirty);
        }
        let base = self.base_ref().await?;
        if run(path, &["merge-base", "--is-ancestor", &base, "HEAD"])
            .await?
            .ok
        {
            return Ok(SyncResult::UpToDate);
        }
        match self.merge_in(path, &base, message).await? {
            MergeOutcome::Merged { commit } => Ok(SyncResult::Merged { commit }),
            MergeOutcome::Conflict { files, detail } => Ok(SyncResult::Conflict { files, detail }),
        }
    }

    async fn conflict_status(
        &self,
        path: &Path,
        files: &[String],
    ) -> Result<ConflictStatus, VcsError> {
        let merging = run(path, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .await?
            .ok
            || !unmerged_paths(path).await?.is_empty();
        let base = self.base_ref().await?;
        let base_merged = run(path, &["merge-base", "--is-ancestor", &base, "HEAD"])
            .await?
            .ok;
        let mut markers = Vec::new();
        if !files.is_empty() {
            // Lines git writes at the start and end of a conflict, in the working tree.
            let mut args = vec![
                "grep",
                "-l",
                "-E",
                "-e",
                "^(<<<<<<<|>>>>>>>) ",
                "--no-index",
                "--",
            ];
            let existing: Vec<&String> = files.iter().filter(|f| path.join(f).exists()).collect();
            if !existing.is_empty() {
                args.extend(existing.iter().map(|f| f.as_str()));
                let found = run(path, &args).await?;
                markers = found
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
            }
        }
        Ok(ConflictStatus {
            merging,
            base_merged,
            markers,
        })
    }

    async fn fast_forward(&self, branch: &str, to: &str) -> Result<FastForward, VcsError> {
        if to == self.base
            && self.remote_ref(to).await?.is_none()
            && !self.branch_exists(to).await?
        {
            self.ensure_branches(&[]).await?;
        }
        let target = match (to == self.base, self.remote_ref(to).await?) {
            (true, Some(sha)) => sha,
            _ => self.head(to).await?,
        };
        if !self.branch_exists(branch).await? {
            git(&self.root, &["branch", branch, &target]).await?;
            return Ok(FastForward::Advanced { commit: target });
        }
        let dir = self.integration_of(branch).await?;
        let before = git(&dir, &["rev-parse", "HEAD"]).await?;
        if before == target {
            return Ok(FastForward::UpToDate { commit: before });
        }
        let moved = run(&dir, &["merge", "--ff-only", "--quiet", &target]).await?;
        if moved.ok {
            return Ok(FastForward::Advanced { commit: target });
        }
        Ok(FastForward::Diverged {
            detail: format!(
                "{branch} has commits that {to} does not, so it cannot be fast-forwarded: {}",
                moved.stderr.trim()
            ),
        })
    }

    async fn land_release(
        &self,
        prod: &str,
        base: &str,
        qa: &str,
        tag: &str,
    ) -> Result<Vec<String>, VcsError> {
        LocalGit::land_release(self, prod, base, qa, tag).await
    }

    async fn merge_into(
        &self,
        target: &str,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        self.merge_branch(target, source, message).await
    }

    async fn commit_file(
        &self,
        branch: &str,
        path: &str,
        contents: &str,
        message: &str,
    ) -> Result<String, VcsError> {
        let relative = Path::new(path);
        let inside = relative.is_relative()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            return Err(VcsError::Io(format!(
                "`{path}` is not a path inside the repository"
            )));
        }
        let holder = self.dirty_holder(branch).await?;
        let dir = match &holder {
            Some(_) => self.scratch_worktree(branch).await?,
            None => self.integration_of(branch).await?,
        };
        let result = self.commit_file_in(&dir, path, contents, message).await;
        let Some(holder) = holder else {
            return result;
        };
        self.drop_scratch(&dir).await;
        let commit = result?;
        self.ff_holder(&holder, branch, &commit).await?;
        Ok(commit)
    }

    async fn tag(&self, name: &str, commit: &str, message: &str) -> Result<(), VcsError> {
        git(&self.root, &["tag", "-a", name, commit, "-m", message]).await?;
        Ok(())
    }

    async fn head(&self, branch: &str) -> Result<String, VcsError> {
        let refname = format!("refs/heads/{branch}");
        git(&self.root, &["rev-parse", "--verify", &refname]).await
    }

    async fn read_file(&self, branch: &str, path: &str) -> Result<Option<String>, VcsError> {
        let spec = format!("refs/heads/{branch}:{path}");
        let shown = run(&self.root, &["show", &spec]).await?;
        Ok(shown.ok.then_some(shown.stdout))
    }

    async fn tag_commit(&self, name: &str) -> Result<Option<String>, VcsError> {
        // `^{commit}` peels an annotated tag to the commit it points at.
        let spec = format!("refs/tags/{name}^{{commit}}");
        let found = run(&self.root, &["rev-parse", "--verify", "--quiet", &spec]).await?;
        Ok(found.ok.then(|| found.stdout.trim().to_string()))
    }

    async fn release_preflight(
        &self,
        qa: &str,
        prod: &str,
        base: &str,
    ) -> Result<Vec<String>, VcsError> {
        self.preflight(qa, prod, base).await
    }

    async fn detached_checkout(&self, name: &str, commit: &str) -> Result<PathBuf, VcsError> {
        let dir = if name == "prod" {
            // One main worktree serves releases and the prod app (ADR 0025).
            self.integration_path_of(self.prod())
        } else {
            self.root
                .join(".chillax/worktrees")
                .join(format!("_{name}"))
        };
        if dir.join(".git").exists() {
            let head = git(&dir, &["rev-parse", "HEAD"]).await.unwrap_or_default();
            if head != commit {
                git(
                    &dir,
                    &["checkout", "--detach", "--force", "--quiet", commit],
                )
                .await?;
            }
            self.share_files(&dir).await;
            return Ok(dir);
        }
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| VcsError::Io(e.to_string()))?;
        }
        let path = dir.to_string_lossy().to_string();
        // A directory left behind by a crashed run is pruned before it is added again.
        let _ = run(&self.root, &["worktree", "prune"]).await;
        git(
            &self.root,
            &["worktree", "add", "--detach", "--force", &path, commit],
        )
        .await?;
        self.share_files(&dir).await;
        Ok(dir)
    }

    async fn checkout(&self, branch: &str) -> Result<PathBuf, VcsError> {
        if !self.branch_exists(branch).await? {
            return Err(VcsError::Git {
                command: "checkout".into(),
                detail: format!("there is no branch `{branch}`"),
            });
        }
        let own = self.integration_path_of(branch);
        if own.join(".git").exists() {
            return Ok(own);
        }
        match self.checkout_of(branch).await? {
            Some(existing) => Ok(existing),
            None => self.integration_of(branch).await,
        }
    }
}

/// Link each of `files` that exists in `root` into the worktree `dir`, and list it in
/// the repository's `info/exclude` (shared by every worktree) so the link never makes a
/// worktree dirty or lands in a commit. A path the worktree already has is left alone.
async fn share_files(root: &Path, dir: &Path, files: &[String]) -> Result<(), VcsError> {
    let io = |e: std::io::Error| VcsError::Io(e.to_string());
    if dir == root {
        return Ok(());
    }
    let present: Vec<&String> = files.iter().filter(|f| root.join(f).is_file()).collect();
    if present.is_empty() {
        return Ok(());
    }
    let common = git(root, &["rev-parse", "--git-common-dir"]).await?;
    let info = root.join(common.trim()).join("info");
    tokio::fs::create_dir_all(&info).await.map_err(io)?;
    let exclude = info.join("exclude");
    let mut listed = tokio::fs::read_to_string(&exclude)
        .await
        .unwrap_or_default();
    let before = listed.len();
    for file in &present {
        let pattern = format!("/{file}");
        if !listed.lines().any(|l| l.trim() == pattern) {
            if !listed.is_empty() && !listed.ends_with('\n') {
                listed.push('\n');
            }
            listed.push_str(&pattern);
            listed.push('\n');
        }
    }
    if listed.len() != before {
        tokio::fs::write(&exclude, listed).await.map_err(io)?;
    }
    for file in present {
        let link = dir.join(file);
        if tokio::fs::symlink_metadata(&link).await.is_ok() {
            continue;
        }
        if let Some(parent) = link.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io)?;
        }
        #[cfg(unix)]
        tokio::fs::symlink(root.join(file), &link)
            .await
            .map_err(io)?;
        #[cfg(not(unix))]
        tokio::fs::copy(root.join(file), &link).await.map_err(io)?;
    }
    Ok(())
}

/// The paths git reports as unmerged in `dir`, mid-merge or mid-rebase.
async fn unmerged_paths(dir: &Path) -> Result<Vec<String>, VcsError> {
    let out = run(dir, &["diff", "--name-only", "--diff-filter=U"]).await?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn conflict_detail(files: &[String]) -> String {
    format!("conflicts in:\n{}", files.join("\n"))
}

/// The branch checked out at `dir`; `None` when detached.
async fn current_branch(dir: &Path) -> Result<Option<String>, VcsError> {
    let out = run(dir, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    Ok(out.ok.then(|| out.stdout.trim().to_string()))
}

pub(crate) fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        // Never prompt, never open an editor.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        // Merge commits and rebased commits are Chillax's doing.
        .env("GIT_COMMITTER_NAME", "Chillax")
        .env("GIT_COMMITTER_EMAIL", "chillax@localhost")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// Lock files older than this belong to a killed process, not a running one.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60);
/// Pauses between retries of a git command that hit a held lock.
const LOCK_BACKOFF_MS: [u64; 5] = [100, 200, 400, 800, 1500];

/// One async mutex per working directory, so Chillax never runs two git
/// commands at once in the same worktree (they would fight over `index.lock`).
fn dir_lock(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    map.entry(dir.to_path_buf()).or_default().clone()
}

/// True when git failed because another process holds a lock file.
fn is_lock_error(stderr: &str) -> bool {
    stderr.contains("index.lock") && stderr.contains("File exists")
        || stderr.contains("Another git process seems to be running")
        || stderr.contains("another git process seems to be running")
}

/// The `.lock` path git names in "Unable to create '<path>.lock': File exists".
fn lock_path(stderr: &str) -> Option<PathBuf> {
    let start = stderr.find("Unable to create '")? + "Unable to create '".len();
    let rest = &stderr[start..];
    let path = &rest[..rest.find('\'')?];
    path.ends_with(".lock").then(|| PathBuf::from(path))
}

/// Remove the lock git complained about when it is old enough to be stale.
fn clear_stale_lock(stderr: &str, max_age: Duration) -> bool {
    let Some(path) = lock_path(stderr) else {
        return false;
    };
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    match age {
        Some(age) if age >= max_age => {
            tracing::warn!(lock = %path.display(), ?age, "removing stale git lock");
            std::fs::remove_file(&path).is_ok()
        }
        _ => false,
    }
}

pub(crate) async fn output(what: &str, mut cmd: Command) -> Result<Output, VcsError> {
    let is_git = cmd.as_std().get_program() == "git";
    let _guard = match cmd.as_std().get_current_dir() {
        Some(dir) if is_git => Some(dir_lock(dir).lock_owned().await),
        _ => None,
    };
    let mut attempt = 0;
    loop {
        let out = tokio::time::timeout(TIMEOUT, cmd.output())
            .await
            .map_err(|_| VcsError::Timeout(what.to_string()))?
            .map_err(|e| VcsError::Io(e.to_string()))?;
        let result = Output {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        };
        if result.ok || !is_git || !is_lock_error(&result.stderr) {
            return Ok(result);
        }
        // An old lock is a killed process's leftover; a fresh one may be an
        // agent mid-command, so wait for it first.
        if clear_stale_lock(&result.stderr, STALE_LOCK_AGE) {
            continue;
        }
        let Some(ms) = LOCK_BACKOFF_MS.get(attempt) else {
            return Ok(result);
        };
        attempt += 1;
        tracing::debug!(what, attempt, "git lock held; retrying");
        tokio::time::sleep(Duration::from_millis(*ms)).await;
    }
}

/// Run git; a non-zero exit is data, not an error.
pub(crate) async fn run(dir: &Path, args: &[&str]) -> Result<Output, VcsError> {
    output(args.first().copied().unwrap_or("git"), command(dir, args)).await
}

pub(crate) fn git_error(what: &str, out: &Output) -> VcsError {
    let detail = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    VcsError::Git {
        command: what.to_string(),
        detail: detail.to_string(),
    }
}

/// Run git and return stdout untouched; a non-zero exit is an error.
async fn git_raw(dir: &Path, args: &[&str]) -> Result<String, VcsError> {
    let out = run(dir, args).await?;
    if out.ok {
        Ok(out.stdout)
    } else {
        Err(git_error(args.first().copied().unwrap_or("git"), &out))
    }
}

/// Run git and return trimmed stdout.
pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<String, VcsError> {
    Ok(git_raw(dir, args).await?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo on `main` with `dev` cut from its first commit.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        sh(dir.path(), &["init", "--quiet", "-b", "main"]);
        write(dir.path(), "README.md", "hello\n");
        // What `chillax init` does, so worktrees do not count as local changes.
        write(dir.path(), ".gitignore", ".chillax/worktrees/\n");
        commit(dir.path(), "initial");
        sh(dir.path(), &["branch", "dev"]);
        dir
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

    fn write(dir: &Path, file: &str, body: &str) {
        std::fs::write(dir.join(file), body).expect("write");
    }

    fn commit(dir: &Path, message: &str) {
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "--quiet", "-m", message]);
    }

    fn vcs(repo: &tempfile::TempDir) -> LocalGit {
        LocalGit::new(repo.path(), "dev")
    }

    fn tree(repo: &tempfile::TempDir, name: &str) -> PathBuf {
        repo.path().join(".chillax/worktrees").join(name)
    }

    async fn feature(repo: &tempfile::TempDir, git: &LocalGit, name: &str, file: &str) -> PathBuf {
        let path = tree(repo, name);
        git.create_worktree(&format!("feat/{name}"), &path)
            .await
            .expect("worktree");
        write(&path, file, &format!("{name}\n"));
        git.commit_all(&path, &format!("{name}: add {file}"))
            .await
            .expect("commit")
            .expect("something to commit");
        path
    }

    #[tokio::test]
    async fn retries_when_a_fresh_index_lock_is_released() {
        let repo = repo();
        let lock = repo.path().join(".git/index.lock");
        std::fs::write(&lock, "").expect("lock");
        let l = lock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::fs::remove_file(l).expect("unlock");
        });
        write(repo.path(), "a.txt", "a\n");
        git(repo.path(), &["add", "a.txt"]).await.expect("retried");
    }

    #[tokio::test]
    async fn clears_a_stale_index_lock() {
        let repo = repo();
        let lock = repo.path().join(".git/index.lock");
        let f = std::fs::File::create(&lock).expect("lock");
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(600))
            .expect("age");
        write(repo.path(), "a.txt", "a\n");
        git(repo.path(), &["add", "a.txt"]).await.expect("cleared");
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn leaves_a_fresh_lock_that_never_clears() {
        let repo = repo();
        let lock = repo.path().join(".git/index.lock");
        std::fs::write(&lock, "").expect("lock");
        write(repo.path(), "a.txt", "a\n");
        assert!(git(repo.path(), &["add", "a.txt"]).await.is_err());
        assert!(lock.exists());
    }

    #[tokio::test]
    async fn a_worktree_gets_its_own_branch_cut_from_the_base() {
        let repo = repo();
        let git = vcs(&repo);
        let path = tree(&repo, "T-1");
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        assert!(path.join("README.md").exists());
        assert_eq!(sh(&path, &["branch", "--show-current"]), "feat/T-1");
        // Asking again, as after a restart, is fine.
        git.create_worktree("feat/T-1", &path).await.expect("again");
    }

    fn shared(repo: &tempfile::TempDir) -> LocalGit {
        vcs(repo).with_shared_files(vec![".env".into(), ".env.local".into()])
    }

    #[tokio::test]
    async fn worktrees_link_the_roots_env_file_without_getting_dirty() {
        let repo = repo();
        write(repo.path(), ".env", "FOO=1\n");
        let git = shared(&repo);
        let path = tree(&repo, "T-1");
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        let link = path.join(".env");
        assert_eq!(
            std::fs::read_link(&link).expect("a link"),
            repo.path().join(".env")
        );
        assert!(!path.join(".env.local").exists(), "only files that exist");
        // An edit in one place shows up everywhere.
        write(repo.path(), ".env", "FOO=2\n");
        assert_eq!(std::fs::read_to_string(&link).expect("read"), "FOO=2\n");
        assert!(git.is_clean(&path).await.expect("status"));
        assert!(git.is_clean(repo.path()).await.expect("status"));
        assert_eq!(
            git.commit_all(&path, "nothing").await.expect("commit"),
            None
        );
        git.create_worktree("feat/T-1", &path).await.expect("again");
        let exclude =
            std::fs::read_to_string(repo.path().join(".git/info/exclude")).expect("exclude");
        assert_eq!(exclude.lines().filter(|l| *l == "/.env").count(), 1);
    }

    #[tokio::test]
    async fn worktrees_made_before_sharing_get_the_link_on_their_next_use() {
        let repo = repo();
        let path = tree(&repo, "T-1");
        vcs(&repo)
            .create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        write(repo.path(), ".env", "FOO=1\n");
        assert!(!path.join(".env").exists());
        shared(&repo)
            .create_worktree("feat/T-1", &path)
            .await
            .expect("reuse");
        assert!(path.join(".env").exists());
    }

    #[tokio::test]
    async fn a_file_the_worktree_already_has_is_left_alone() {
        let repo = repo();
        write(repo.path(), ".env", "FOO=tracked\n");
        commit(repo.path(), "track .env");
        sh(repo.path(), &["branch", "-f", "dev"]);
        let git = shared(&repo);
        let path = tree(&repo, "T-1");
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        assert!(
            !std::fs::symlink_metadata(path.join(".env"))
                .expect("exists")
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn the_prod_checkout_links_the_env_file_too() {
        let repo = repo();
        write(repo.path(), ".env", "FOO=1\n");
        let head = sh(repo.path(), &["rev-parse", "HEAD"]);
        let dir = shared(&repo)
            .detached_checkout("prod", &head)
            .await
            .expect("prod");
        assert!(dir.join(".env").exists());
    }

    #[tokio::test]
    async fn a_missing_base_branch_is_created_from_head() {
        let repo = repo();
        sh(repo.path(), &["branch", "-D", "dev"]);
        let git = vcs(&repo);
        let path = tree(&repo, "T-1");
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        assert!(path.join("README.md").exists());
        assert_eq!(
            sh(repo.path(), &["rev-parse", "dev"]),
            sh(repo.path(), &["rev-parse", "main"])
        );
    }

    #[tokio::test]
    async fn a_leftover_branch_is_reused_after_its_worktree_is_gone() {
        let repo = repo();
        let git = vcs(&repo);
        let path = feature(&repo, &git, "T-1", "a.txt").await;
        git.remove_worktree(&path).await.expect("remove");
        assert!(!path.exists());
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("recreate");
        assert!(path.join("a.txt").exists(), "earlier commits are kept");
    }

    #[tokio::test]
    async fn commit_all_commits_once_and_reports_nothing_to_commit() {
        let repo = repo();
        let git = vcs(&repo);
        let path = tree(&repo, "T-1");
        git.create_worktree("feat/T-1", &path)
            .await
            .expect("create");
        assert_eq!(git.commit_all(&path, "empty").await.expect("ok"), None);
        write(&path, "a.txt", "a\n");
        assert!(!git.is_clean(&path).await.expect("status"));
        let sha = git
            .commit_all(&path, "T-1: add a")
            .await
            .expect("commit")
            .expect("committed");
        assert_eq!(sha, sh(&path, &["rev-parse", "HEAD"]));
        assert!(git.is_clean(&path).await.expect("status"));
    }

    #[tokio::test]
    async fn diff_shows_the_branch_and_is_capped() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        let diff = git.diff("feat/T-1", 10_000).await.expect("diff");
        assert!(diff.contains("+T-1"), "{diff}");
        let short = git.diff("feat/T-1", 20).await.expect("diff");
        assert!(short.contains("(diff truncated)"));
        assert!(short.len() < diff.len());
    }

    #[tokio::test]
    async fn merging_advances_the_base_without_touching_the_main_checkout() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        let outcome = git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        let MergeOutcome::Merged { commit } = outcome else {
            panic!("expected a merge, got {outcome:?}");
        };
        assert_eq!(commit, sh(repo.path(), &["rev-parse", "dev"]));
        assert_eq!(sh(repo.path(), &["branch", "--show-current"]), "main");
        assert!(!repo.path().join("a.txt").exists());
        assert_eq!(
            sh(repo.path(), &["show", "dev:a.txt"]),
            "T-1",
            "dev has the file"
        );
        assert!(git.integration_path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn merges_stack_in_the_same_integration_worktree() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        feature(&repo, &git, "T-2", "b.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("first");
        git.merge("feat/T-2", "Merge T-2").await.expect("second");
        let files = sh(repo.path(), &["ls-tree", "--name-only", "dev"]);
        assert!(
            files.contains("a.txt") && files.contains("b.txt"),
            "{files}"
        );
    }

    #[tokio::test]
    async fn a_conflicting_merge_is_aborted_and_leaves_the_base_alone() {
        let repo = repo();
        let git = vcs(&repo);
        let one = feature(&repo, &git, "T-1", "same.txt").await;
        feature(&repo, &git, "T-2", "same.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("first");
        let before = sh(repo.path(), &["rev-parse", "dev"]);
        let outcome = git.merge("feat/T-2", "Merge T-2").await.expect("merge");
        let MergeOutcome::Conflict { detail, .. } = outcome else {
            panic!("expected a conflict, got {outcome:?}");
        };
        assert!(detail.contains("same.txt"), "{detail}");
        assert_eq!(sh(repo.path(), &["rev-parse", "dev"]), before);
        // The integration worktree is usable again straight away.
        assert!(git.is_clean(&git.integration_path()).await.expect("clean"));
        assert!(one.exists());
    }

    #[tokio::test]
    async fn merging_an_unknown_branch_is_an_error_not_a_conflict() {
        let repo = repo();
        let git = vcs(&repo);
        assert!(git.merge("feat/nope", "Merge").await.is_err());
    }

    #[tokio::test]
    async fn the_base_checked_out_in_the_repository_is_merged_in_place_when_clean() {
        let repo = repo();
        sh(repo.path(), &["checkout", "--quiet", "dev"]);
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        assert!(repo.path().join("a.txt").exists());

        // An unrelated dirty file in the user's checkout does not block the merge.
        write(repo.path(), "scratch.txt", "dirty\n");
        write(repo.path(), "README.md", "edited\n");
        feature(&repo, &git, "T-2", "b.txt").await;
        git.merge("feat/T-2", "Merge T-2").await.expect("merge");
        assert_eq!(
            sh(repo.path(), &["rev-parse", "dev"]),
            sh(repo.path(), &["rev-parse", "HEAD"])
        );
        assert!(repo.path().join("b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("scratch.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
            "edited\n"
        );
    }

    #[tokio::test]
    async fn a_dirty_file_the_merge_changes_blocks_it_without_losing_anything() {
        let repo = repo();
        sh(repo.path(), &["checkout", "--quiet", "dev"]);
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        let before = sh(repo.path(), &["rev-parse", "dev"]);
        write(repo.path(), "a.txt", "mine\n"); // untracked here, but the merge adds it
        let err = git.merge("feat/T-1", "Merge T-1").await.expect_err("busy");
        assert!(matches!(err, VcsError::BaseBusy(_)), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).unwrap(),
            "mine\n"
        );
        assert_eq!(sh(repo.path(), &["rev-parse", "dev"]), before);
        assert!(
            !repo
                .path()
                .join(".chillax/worktrees")
                .read_dir()
                .is_ok_and(|mut d| d.any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("_merge")))
        );
    }

    #[tokio::test]
    async fn rebase_moves_an_open_branch_onto_the_new_base() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        let two = feature(&repo, &git, "T-2", "b.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        assert_eq!(
            git.rebase(&two).await.expect("rebase"),
            RebaseResult::Rebased
        );
        assert!(two.join("a.txt").exists(), "T-2 now includes T-1");
        assert!(two.join("b.txt").exists());
    }

    #[tokio::test]
    async fn rebase_leaves_dirty_trees_alone() {
        let repo = repo();
        let git = vcs(&repo);
        let one = feature(&repo, &git, "T-1", "a.txt").await;
        write(&one, "wip.txt", "unfinished\n");
        assert_eq!(git.rebase(&one).await.expect("rebase"), RebaseResult::Dirty);
        assert!(one.join("wip.txt").exists());
    }

    #[tokio::test]
    async fn a_conflicting_rebase_is_aborted() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "same.txt").await;
        let two = feature(&repo, &git, "T-2", "same.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        let before = sh(&two, &["rev-parse", "HEAD"]);
        let result = git.rebase(&two).await.expect("rebase");
        assert!(
            matches!(result, RebaseResult::Conflict { .. }),
            "{result:?}"
        );
        assert_eq!(sh(&two, &["rev-parse", "HEAD"]), before);
        assert!(
            git.is_clean(&two).await.expect("clean"),
            "rebase was aborted"
        );
    }

    #[tokio::test]
    async fn a_conflicting_merge_and_rebase_name_their_files() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "same.txt").await;
        let two = feature(&repo, &git, "T-2", "same.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("first");
        let merged = git.merge("feat/T-2", "Merge T-2").await.expect("merge");
        assert!(
            matches!(&merged, MergeOutcome::Conflict { files, .. } if files == &["same.txt"]),
            "{merged:?}"
        );
        let rebased = git.rebase(&two).await.expect("rebase");
        assert!(
            matches!(&rebased, RebaseResult::Conflict { files, .. } if files == &["same.txt"]),
            "{rebased:?}"
        );
    }

    #[tokio::test]
    async fn a_branch_that_has_the_base_is_up_to_date() {
        let repo = repo();
        let git = vcs(&repo);
        let one = feature(&repo, &git, "T-1", "a.txt").await;
        let before = sh(&one, &["rev-parse", "HEAD"]);
        assert_eq!(
            git.sync_with_base(&one, "Merge dev").await.expect("sync"),
            SyncResult::UpToDate
        );
        assert_eq!(sh(&one, &["rev-parse", "HEAD"]), before);
    }

    #[tokio::test]
    async fn syncing_merges_new_base_commits_into_the_branch() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "a.txt").await;
        let two = feature(&repo, &git, "T-2", "b.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        let synced = git.sync_with_base(&two, "Merge dev").await.expect("sync");
        let SyncResult::Merged { commit } = synced else {
            panic!("expected a merge, got {synced:?}");
        };
        assert_eq!(sh(&two, &["rev-parse", "HEAD"]), commit);
        assert!(two.join("a.txt").exists(), "T-2 now includes T-1");
        assert_eq!(
            sh(&two, &["rev-list", "--parents", "-n1", "HEAD"])
                .split(' ')
                .count(),
            3,
            "a merge commit"
        );
    }

    #[tokio::test]
    async fn a_conflicting_sync_is_aborted_and_names_the_files() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "same.txt").await;
        let two = feature(&repo, &git, "T-2", "same.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        let before = sh(&two, &["rev-parse", "HEAD"]);
        let synced = git.sync_with_base(&two, "Merge dev").await.expect("sync");
        assert!(
            matches!(&synced, SyncResult::Conflict { files, .. } if files == &["same.txt"]),
            "{synced:?}"
        );
        assert_eq!(sh(&two, &["rev-parse", "HEAD"]), before);
        assert!(git.is_clean(&two).await.expect("clean"), "merge aborted");
        write(&two, "wip.txt", "x\n");
        assert_eq!(
            git.sync_with_base(&two, "Merge dev").await.expect("sync"),
            SyncResult::Dirty
        );
    }

    #[tokio::test]
    async fn conflict_status_follows_a_resolution_to_the_end() {
        let repo = repo();
        let git = vcs(&repo);
        feature(&repo, &git, "T-1", "same.txt").await;
        let two = feature(&repo, &git, "T-2", "same.txt").await;
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        let files = vec!["same.txt".to_string()];
        let status = git.conflict_status(&two, &files).await.expect("status");
        assert!(!status.base_merged && !status.resolved(), "{status:?}");
        // The agent starts the merge and stops halfway.
        let merging = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["merge", "dev"])
            .current_dir(&two)
            .output()
            .expect("git runs");
        assert!(!merging.status.success());
        let status = git.conflict_status(&two, &files).await.expect("status");
        assert!(status.merging, "{status:?}");
        assert_eq!(status.markers, files);
        // Markers committed by mistake are still found.
        commit(&two, "merge dev with markers");
        let status = git.conflict_status(&two, &files).await.expect("status");
        assert!(!status.merging && status.base_merged, "{status:?}");
        assert_eq!(status.markers, files);
        assert!(!status.resolved());
        write(&two, "same.txt", "T-1\nT-2\n");
        commit(&two, "resolve");
        let status = git.conflict_status(&two, &files).await.expect("status");
        assert!(status.resolved(), "{status:?}");
    }

    #[tokio::test]
    async fn one_worktree_moves_between_ticket_branches_and_dev_and_merges_on_dev() {
        let repo = repo();
        let git = vcs(&repo);
        let tree = repo.path().join(INTEGRATION_WORKTREE);
        git.create_worktree("feat/T-1", &tree)
            .await
            .expect("create");
        write(&tree, "a.txt", "a\n");
        git.commit_all(&tree, "T-1").await.expect("commit");
        // Back to dev, then a second ticket's branch is cut from dev.
        git.switch_branch(&tree, "dev", None).await.expect("dev");
        assert_eq!(sh(&tree, &["branch", "--show-current"]), "dev");
        assert!(!tree.join("a.txt").exists());
        git.switch_branch(&tree, "feat/T-2", Some("dev"))
            .await
            .expect("create");
        assert_eq!(sh(&tree, &["branch", "--show-current"]), "feat/T-2");
        // A branch that is missing without a start point is an error.
        assert!(git.switch_branch(&tree, "nope", None).await.is_err());
        // Merging a branch moves the worktree to dev first.
        git.merge("feat/T-1", "Merge T-1").await.expect("merge");
        assert_eq!(sh(&tree, &["branch", "--show-current"]), "dev");
        assert!(tree.join("a.txt").exists());
        git.switch_branch(&tree, "feat/T-1", Some("dev"))
            .await
            .expect("back");
        assert_eq!(sh(&tree, &["branch", "--show-current"]), "feat/T-1");
    }

    #[tokio::test]
    async fn a_release_merge_after_a_detached_prod_checkout_reuses_the_main_worktree() {
        let (repo, git) = release_repo();
        let first = git.head("dev").await.expect("head");
        let detached = git.detached_checkout("prod", &first).await.expect("prod");
        let merged = git
            .merge_into("main", "dev", "Release")
            .await
            .expect("merge");
        assert!(matches!(merged, MergeOutcome::Merged { .. }), "{merged:?}");
        assert_eq!(git.checkout("main").await.expect("main"), detached);
        assert_eq!(sh(&detached, &["branch", "--show-current"]), "main");
        let _ = repo;
    }

    // ---- QA and release branches ----

    /// `dev` is checked out in the repository, so `main` and `qa` are free for Chillax.
    fn release_repo() -> (tempfile::TempDir, LocalGit) {
        let repo = repo();
        sh(repo.path(), &["checkout", "--quiet", "dev"]);
        let git = vcs(&repo);
        (repo, git)
    }

    async fn merged_feature(repo: &tempfile::TempDir, git: &LocalGit, name: &str, file: &str) {
        feature(repo, git, name, file).await;
        git.merge(&format!("feat/{name}"), &format!("Merge {name}"))
            .await
            .expect("merge");
    }

    #[tokio::test]
    async fn fast_forward_creates_the_branch_then_follows_dev() {
        let (repo, git) = release_repo();
        let dev = sh(repo.path(), &["rev-parse", "dev"]);
        let first = git.fast_forward("qa", "dev").await.expect("create");
        assert_eq!(
            first,
            FastForward::Advanced {
                commit: dev.clone()
            }
        );
        assert_eq!(
            git.fast_forward("qa", "dev").await.expect("same"),
            FastForward::UpToDate { commit: dev }
        );

        merged_feature(&repo, &git, "T-1", "a.txt").await;
        let tip = sh(repo.path(), &["rev-parse", "dev"]);
        let advanced = git.fast_forward("qa", "dev").await.expect("advance");
        assert_eq!(
            advanced,
            FastForward::Advanced {
                commit: tip.clone()
            }
        );
        assert_eq!(git.head("qa").await.expect("head"), tip);
        assert_eq!(sh(repo.path(), &["branch", "--show-current"]), "dev");
    }

    #[tokio::test]
    async fn a_diverged_qa_is_reported_and_left_alone() {
        let (repo, git) = release_repo();
        git.fast_forward("qa", "dev").await.expect("create");
        git.commit_file("qa", "CHANGELOG.md", "# qa only\n", "notes")
            .await
            .expect("commit on qa");
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        let before = git.head("qa").await.expect("head");
        let result = git.fast_forward("qa", "dev").await.expect("ff");
        assert!(matches!(result, FastForward::Diverged { .. }), "{result:?}");
        assert_eq!(git.head("qa").await.expect("head"), before);
    }

    #[tokio::test]
    async fn commit_file_on_a_dirty_checked_out_branch_keeps_the_local_changes() {
        let (repo, git) = release_repo();
        sh(repo.path(), &["checkout", "--quiet", "dev"]);
        write(repo.path(), "scratch.txt", "dirty\n");
        let commit = git
            .commit_file("dev", "docs/CHANGELOG.md", "# v1\n", "docs: notes")
            .await
            .expect("commit");
        assert_eq!(commit, git.head("dev").await.expect("head"));
        assert!(repo.path().join("docs/CHANGELOG.md").exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("scratch.txt")).unwrap(),
            "dirty\n"
        );
    }

    #[tokio::test]
    async fn commit_file_commits_on_the_branch_without_touching_the_checkout() {
        let (repo, git) = release_repo();
        git.fast_forward("qa", "dev").await.expect("create");
        let commit = git
            .commit_file(
                "qa",
                "docs/CHANGELOG.md",
                "# v0.1.0\n",
                "docs: v0.1.0 notes",
            )
            .await
            .expect("commit");
        assert_eq!(commit, git.head("qa").await.expect("head"));
        assert_eq!(
            sh(repo.path(), &["show", "qa:docs/CHANGELOG.md"]),
            "# v0.1.0"
        );
        assert!(!repo.path().join("docs").exists());
        // Paths stay inside the repository.
        assert!(git.commit_file("qa", "../out.txt", "x", "m").await.is_err());
        assert!(git.commit_file("qa", "/etc/x", "x", "m").await.is_err());
    }

    #[tokio::test]
    async fn merge_into_merges_one_branch_into_another() {
        let (repo, git) = release_repo();
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        git.fast_forward("qa", "dev").await.expect("qa");
        let outcome = git
            .merge_into("main", "qa", "Release v0.1.0")
            .await
            .expect("merge");
        let MergeOutcome::Merged { commit } = outcome else {
            panic!("expected a merge, got {outcome:?}");
        };
        assert_eq!(commit, git.head("main").await.expect("head"));
        assert_eq!(sh(repo.path(), &["show", "main:a.txt"]), "T-1");
        assert_eq!(
            sh(repo.path(), &["log", "-1", "--format=%s", "main"]),
            "Release v0.1.0"
        );
    }

    #[tokio::test]
    async fn merge_into_a_checked_out_branch_leaves_unrelated_changes_alone() {
        let repo = repo();
        let git = vcs(&repo);
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        git.fast_forward("qa", "dev").await.expect("qa");
        // `main` is the repository's own checkout, with local changes.
        write(repo.path(), "scratch.txt", "dirty\n");
        git.merge_into("main", "qa", "Release")
            .await
            .expect("merged");
        assert!(repo.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(repo.path().join("scratch.txt")).unwrap(),
            "dirty\n"
        );
    }

    #[tokio::test]
    async fn tags_are_annotated_and_never_overwritten() {
        let (repo, git) = release_repo();
        let dev = git.head("dev").await.expect("head");
        git.tag("v0.1.0", &dev, "Release v0.1.0\n\nNotes here")
            .await
            .expect("tag");
        assert_eq!(sh(repo.path(), &["cat-file", "-t", "v0.1.0"]), "tag");
        assert!(sh(repo.path(), &["tag", "-n9", "-l", "v0.1.0"]).contains("Notes here"));
        assert_eq!(sh(repo.path(), &["rev-list", "-n1", "v0.1.0"]), dev);
        assert!(git.tag("v0.1.0", &dev, "again").await.is_err());
    }

    #[tokio::test]
    async fn a_release_flows_qa_to_main_then_back_into_dev() {
        let (repo, git) = release_repo();
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        git.fast_forward("qa", "dev").await.expect("promote");
        git.commit_file("qa", "CHANGELOG.md", "# v0.1.0\n", "docs: v0.1.0")
            .await
            .expect("notes");
        let MergeOutcome::Merged { commit } = git
            .merge_into("main", "qa", "Release v0.1.0")
            .await
            .expect("release")
        else {
            panic!("expected a merge");
        };
        git.tag("v0.1.0", &commit, "Release v0.1.0")
            .await
            .expect("tag");
        // Main goes back into dev, so qa stays an ancestor of dev.
        git.merge_into("dev", "main", "Merge release v0.1.0 back")
            .await
            .expect("back-merge");
        merged_feature(&repo, &git, "T-2", "b.txt").await;
        let next = git.fast_forward("qa", "dev").await.expect("next promotion");
        assert!(matches!(next, FastForward::Advanced { .. }), "{next:?}");
        assert_eq!(sh(repo.path(), &["show", "qa:b.txt"]), "T-2");
        assert_eq!(sh(repo.path(), &["show", "qa:CHANGELOG.md"]), "# v0.1.0");
    }

    #[tokio::test]
    async fn files_and_tags_can_be_read_back() {
        let (repo, git) = release_repo();
        git.fast_forward("qa", "dev").await.expect("qa");
        assert_eq!(
            git.read_file("qa", "CHANGELOG.md").await.expect("read"),
            None
        );
        git.commit_file("qa", "CHANGELOG.md", "# Changelog\n", "notes")
            .await
            .expect("commit");
        assert_eq!(
            git.read_file("qa", "CHANGELOG.md")
                .await
                .expect("read")
                .as_deref(),
            Some("# Changelog\n")
        );
        assert_eq!(
            git.read_file("nope", "CHANGELOG.md").await.expect("read"),
            None
        );

        let head = git.head("qa").await.expect("head");
        assert_eq!(git.tag_commit("v0.1.0").await.expect("lookup"), None);
        git.tag("v0.1.0", &head, "Release").await.expect("tag");
        // The tag answers with the commit it points at, not the tag object.
        assert_eq!(git.tag_commit("v0.1.0").await.expect("lookup"), Some(head));
        let _ = repo;
    }

    #[tokio::test]
    async fn a_detached_checkout_follows_the_commit_and_keeps_untracked_files() {
        let (repo, git) = release_repo();
        let first = git.head("dev").await.expect("head");
        let dir = git.detached_checkout("prod", &first).await.expect("create");
        // Prod shares the main worktree (ADR 0025).
        assert_eq!(dir, repo.path().join(".chillax/worktrees/_main"));
        assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), first);
        assert_eq!(sh(&dir, &["branch", "--show-current"]), "");
        write(&dir, "node_modules.txt", "installed\n");
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        let second = git.head("dev").await.expect("head");
        let again = git.detached_checkout("prod", &second).await.expect("move");
        assert_eq!(again, dir);
        assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), second);
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("node_modules.txt").exists(), "untracked kept");
        // The branch checkouts are untouched by it.
        assert_eq!(git.head("dev").await.expect("head"), second);
    }

    #[tokio::test]
    async fn a_branch_is_found_where_it_is_checked_out_or_in_its_own_worktree() {
        let (repo, git) = release_repo();
        // dev is the repository's own checkout.
        let same = std::fs::canonicalize(repo.path()).expect("canonical");
        let found = git.checkout("dev").await.expect("dev");
        assert_eq!(std::fs::canonicalize(found).expect("canonical"), same);
        // qa has no checkout yet, so it gets its integration worktree.
        git.fast_forward("qa", "dev").await.expect("qa");
        let qa = git.checkout("qa").await.expect("qa");
        assert_eq!(qa, repo.path().join(".chillax/worktrees/_qa"));
        assert!(qa.join("README.md").exists());
        assert_eq!(git.checkout("qa").await.expect("again"), qa);
        assert!(git.checkout("nope").await.is_err());
    }

    #[tokio::test]
    async fn preflight_is_quiet_when_a_release_would_go_through() {
        let repo = repo();
        let git = vcs(&repo);
        git.fast_forward("qa", "dev").await.expect("qa");
        let problems = git
            .release_preflight("qa", "main", "dev")
            .await
            .expect("ok");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[tokio::test]
    async fn preflight_names_a_main_with_commits_qa_lacks() {
        let repo = repo();
        let git = vcs(&repo);
        git.fast_forward("qa", "dev").await.expect("qa");
        // Someone committed to main directly.
        write(repo.path(), "hotfix.txt", "fix\n");
        commit(repo.path(), "hotfix on main");
        let problems = git
            .release_preflight("qa", "main", "dev")
            .await
            .expect("ok");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("`main` has commits that `qa` lacks"));
        assert!(problems[0].contains("git merge main"));
    }

    #[tokio::test]
    async fn preflight_ignores_a_dirty_checkout_because_merges_do_not_need_it_clean() {
        let repo = repo();
        let git = vcs(&repo);
        git.fast_forward("qa", "dev").await.expect("qa");
        write(repo.path(), "README.md", "edited\n");
        let problems = git
            .release_preflight("qa", "main", "dev")
            .await
            .expect("ok");
        assert!(problems.is_empty(), "{problems:?}");
    }

    // ---- ensure_branches ----

    fn bare_repo(initial: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        sh(dir.path(), &["init", "--quiet", "--bare", "-b", initial]);
        dir
    }

    fn unborn(branch: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        sh(dir.path(), &["init", "--quiet", "-b", branch]);
        dir
    }

    fn sha(repo: &tempfile::TempDir, rev: &str) -> String {
        sh(repo.path(), &["rev-parse", rev])
    }

    /// No git identity from the machine: a global and system config of nothing.
    fn isolate_git_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: runs once before the test spawns git; the other tests in this
            // crate pass their own identity with `-c`, so they do not read these.
            unsafe {
                std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
                std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            }
        });
    }

    #[tokio::test]
    async fn an_unborn_repo_gets_an_empty_prod_commit_and_dev_and_qa_at_it() {
        isolate_git_config();
        let repo = unborn("main");
        let created = vcs(&repo).ensure_branches(&["qa"]).await.expect("ensure");
        assert_eq!(created, ["main", "dev", "qa"]);
        assert_eq!(
            sh(repo.path(), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        assert_eq!(sha(&repo, "dev"), sha(&repo, "main"));
        assert_eq!(sha(&repo, "qa"), sha(&repo, "main"));
        assert_eq!(sh(repo.path(), &["ls-tree", "-r", "HEAD"]), "");
        assert_eq!(sh(repo.path(), &["rev-list", "--count", "HEAD"]), "1");
    }

    #[tokio::test]
    async fn an_unborn_repo_on_another_default_branch_gets_main() {
        let repo = unborn("trunk");
        vcs(&repo).ensure_branches(&["qa"]).await.expect("ensure");
        assert_eq!(
            sh(repo.path(), &["symbolic-ref", "HEAD"]),
            "refs/heads/main"
        );
        assert_eq!(sha(&repo, "dev"), sha(&repo, "main"));
    }

    #[tokio::test]
    async fn branches_are_cut_from_prod_not_from_a_checked_out_feature_branch() {
        let repo = repo();
        sh(repo.path(), &["branch", "-D", "dev"]);
        sh(repo.path(), &["checkout", "--quiet", "-b", "feat/x"]);
        write(repo.path(), "x.txt", "x\n");
        commit(repo.path(), "x");
        let created = vcs(&repo).ensure_branches(&["qa"]).await.expect("ensure");
        assert_eq!(created, ["dev", "qa"]);
        assert_eq!(sha(&repo, "dev"), sha(&repo, "main"));
        assert_ne!(sha(&repo, "dev"), sha(&repo, "feat/x"));
    }

    #[tokio::test]
    async fn without_a_prod_branch_commits_fall_back_to_head() {
        let repo = unborn("trunk");
        write(repo.path(), "a.txt", "a\n");
        commit(repo.path(), "a");
        let created = vcs(&repo).ensure_branches(&["qa"]).await.expect("ensure");
        assert_eq!(created, ["dev", "qa"]);
        assert_eq!(sha(&repo, "dev"), sha(&repo, "HEAD"));
        assert!(
            !run(
                repo.path(),
                &["rev-parse", "--verify", "--quiet", "refs/heads/main"]
            )
            .await
            .expect("run")
            .ok,
            "prod is not invented"
        );
    }

    #[tokio::test]
    async fn existing_branches_are_left_alone_and_a_second_call_creates_nothing() {
        let repo = repo();
        write(repo.path(), "b.txt", "b\n");
        commit(repo.path(), "b");
        sh(repo.path(), &["branch", "qa", "dev"]);
        let (dev, qa, main) = (sha(&repo, "dev"), sha(&repo, "qa"), sha(&repo, "main"));
        let git = vcs(&repo);
        assert!(
            git.ensure_branches(&["qa"])
                .await
                .expect("ensure")
                .is_empty()
        );
        assert!(
            git.ensure_branches(&["qa"])
                .await
                .expect("ensure")
                .is_empty()
        );
        assert_eq!(
            (sha(&repo, "dev"), sha(&repo, "qa"), sha(&repo, "main")),
            (dev, qa, main)
        );
    }

    #[tokio::test]
    async fn custom_branch_names_are_honoured() {
        let repo = unborn("main");
        let git = LocalGit::new(repo.path(), "develop").with_prod("production");
        let created = git.ensure_branches(&["staging"]).await.expect("ensure");
        assert_eq!(created, ["production", "develop", "staging"]);
        assert_eq!(
            sh(repo.path(), &["symbolic-ref", "HEAD"]),
            "refs/heads/production"
        );
        assert_eq!(sha(&repo, "develop"), sha(&repo, "production"));
        assert_eq!(sha(&repo, "staging"), sha(&repo, "production"));
    }

    #[tokio::test]
    async fn a_base_or_qa_named_like_prod_is_a_no_op() {
        let repo = repo();
        sh(repo.path(), &["branch", "-D", "dev"]);
        let git = LocalGit::new(repo.path(), "main");
        assert!(
            git.ensure_branches(&["main"])
                .await
                .expect("ensure")
                .is_empty()
        );
    }

    /// A bare remote with `main` pushed (and `dev` too when asked), cloned from `repo()`.
    fn with_remote(push_dev: bool) -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = bare_repo("main");
        let local = repo();
        sh(local.path(), &["branch", "-D", "dev"]);
        sh(
            local.path(),
            &["remote", "add", "origin", &origin.path().to_string_lossy()],
        );
        sh(local.path(), &["push", "--quiet", "origin", "main"]);
        if push_dev {
            sh(local.path(), &["branch", "dev"]);
            write(local.path(), "d.txt", "d\n");
            sh(local.path(), &["checkout", "--quiet", "dev"]);
            commit(local.path(), "dev work");
            sh(local.path(), &["push", "--quiet", "origin", "dev"]);
            sh(local.path(), &["checkout", "--quiet", "main"]);
            sh(local.path(), &["branch", "-D", "dev"]);
        }
        (origin, local)
    }

    #[tokio::test]
    async fn a_remote_with_only_main_gives_a_worktree_cut_from_main() {
        let (_origin, repo) = with_remote(false);
        let git = LocalGit::new(repo.path(), "dev").with_remote("origin");
        git.create_worktree("feat/T-1", &tree(&repo, "T-1"))
            .await
            .expect("worktree");
        assert_eq!(sha(&repo, "dev"), sha(&repo, "main"));
    }

    #[tokio::test]
    async fn a_remote_dev_is_used_when_there_is_no_local_dev() {
        let (_origin, repo) = with_remote(true);
        let git = LocalGit::new(repo.path(), "dev").with_remote("origin");
        git.sync_branch("main").await.expect("fetch");
        let created = git.ensure_branches(&[]).await.expect("ensure");
        assert_eq!(created, ["dev"]);
        assert_eq!(sha(&repo, "dev"), sha(&repo, "origin/dev"));
        assert_ne!(sha(&repo, "dev"), sha(&repo, "main"));
    }

    #[tokio::test]
    async fn a_worktree_in_a_repo_with_no_commits_works() {
        let repo = unborn("main");
        let git = vcs(&repo);
        git.create_worktree("feat/T-1", &tree(&repo, "T-1"))
            .await
            .expect("worktree");
        assert_eq!(sha(&repo, "dev"), sha(&repo, "main"));
    }

    // ---- Converging after a deployed release ----

    struct Released {
        origin: tempfile::TempDir,
        repo: tempfile::TempDir,
        git: LocalGit,
    }

    /// A finished release: T-1 merged into `dev`, promoted to `qa`, released into `main`
    /// and tagged `v1`, `dev` merged with `main`. `dev` is checked out in the repository;
    /// the `_main` worktree is detached (as a prod deploy leaves it) unless `hold_main`.
    async fn released(remote: bool, hold_main: bool) -> Released {
        let origin = bare_repo("main");
        let repo = repo();
        let mut git = LocalGit::new(repo.path(), "dev").with_prod("main");
        if remote {
            let url = origin.path().to_string_lossy().to_string();
            sh(repo.path(), &["remote", "add", "origin", &url]);
            sh(repo.path(), &["push", "--quiet", "origin", "main", "dev"]);
            git = git.with_remote("origin");
        }
        sh(repo.path(), &["checkout", "--quiet", "dev"]);
        merged_feature(&repo, &git, "T-1", "a.txt").await;
        git.fast_forward("qa", "dev").await.expect("qa");
        git.merge_into("main", "qa", "Release v1")
            .await
            .expect("release");
        let main = git.head("main").await.expect("main");
        git.tag("v1", &main, "v1").await.expect("tag");
        git.merge_into("dev", "main", "Merge release v1 back into dev")
            .await
            .expect("back");
        if !hold_main {
            sh(&tree(&repo, "_main"), &["switch", "--quiet", "--detach"]);
        }
        Released { origin, repo, git }
    }

    fn remote_sha(r: &Released, rev: &str) -> String {
        sh(r.origin.path(), &["rev-parse", rev])
    }

    #[tokio::test]
    async fn a_deployed_release_converges_qa_and_main_pushes_all_and_switches_to_main() {
        let r = released(true, false).await;
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(sha(&r.repo, "qa"), sha(&r.repo, "main"));
        // dev holds the release and the merge-back commit on top.
        sh(
            r.repo.path(),
            &["merge-base", "--is-ancestor", "main", "dev"],
        );
        for branch in ["dev", "qa", "main"] {
            assert_eq!(remote_sha(&r, branch), sha(&r.repo, branch), "{branch}");
        }
        assert_eq!(remote_sha(&r, "refs/tags/v1"), sha(&r.repo, "refs/tags/v1"));
        assert_eq!(sh(r.repo.path(), &["branch", "--show-current"]), "main");
    }

    #[tokio::test]
    async fn landing_twice_changes_nothing() {
        let r = released(true, false).await;
        r.git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("first");
        let before: Vec<String> = ["dev", "qa", "main"].map(|b| sha(&r.repo, b)).to_vec();
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("second");
        assert!(notes.is_empty(), "{notes:?}");
        let after: Vec<String> = ["dev", "qa", "main"].map(|b| sha(&r.repo, b)).to_vec();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn without_a_remote_the_branches_converge_locally() {
        let r = released(false, false).await;
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(sha(&r.repo, "qa"), sha(&r.repo, "main"));
        assert_eq!(sh(r.repo.path(), &["branch", "--show-current"]), "main");
    }

    #[tokio::test]
    async fn a_dev_behind_main_is_fast_forwarded() {
        let r = released(false, false).await;
        // A dev that never got the back-merge.
        let main = sha(&r.repo, "main");
        let first = sh(r.repo.path(), &["rev-parse", "dev^1"]);
        sh(r.repo.path(), &["reset", "--quiet", "--hard", &first]);
        r.git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(sha(&r.repo, "dev"), main);
    }

    #[tokio::test]
    async fn a_dev_that_diverged_from_main_is_left_alone_with_a_note() {
        let r = released(false, false).await;
        let first = sh(r.repo.path(), &["rev-parse", "dev^1"]);
        sh(r.repo.path(), &["reset", "--quiet", "--hard", &first]);
        write(r.repo.path(), "extra.txt", "x\n");
        commit(r.repo.path(), "new work on dev");
        let dev = sha(&r.repo, "dev");
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(sha(&r.repo, "dev"), dev);
        assert!(notes.iter().any(|n| n.contains("dev")), "{notes:?}");
    }

    #[tokio::test]
    async fn a_dirty_primary_checkout_stays_on_its_branch() {
        let r = released(false, false).await;
        write(r.repo.path(), "wip.txt", "wip\n");
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(sh(r.repo.path(), &["branch", "--show-current"]), "dev");
        assert!(notes.iter().any(|n| n.contains("uncommitted")), "{notes:?}");
    }

    #[tokio::test]
    async fn a_merge_in_progress_in_the_primary_checkout_is_not_disturbed() {
        let r = released(false, false).await;
        let head = sha(&r.repo, "dev");
        let marker = sh(r.repo.path(), &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(Path::new(&marker).join("MERGE_HEAD"), format!("{head}\n")).expect("marker");
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(sh(r.repo.path(), &["branch", "--show-current"]), "dev");
        assert!(notes.iter().any(|n| n.contains("MERGE_HEAD")), "{notes:?}");
    }

    #[tokio::test]
    async fn main_held_by_another_worktree_is_not_taken() {
        let r = released(false, true).await;
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(sh(r.repo.path(), &["branch", "--show-current"]), "dev");
        assert!(
            notes.iter().any(|n| n.contains("checked out at")),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn a_push_the_remote_would_reject_becomes_a_note() {
        let r = released(true, false).await;
        // Someone else moved origin's qa to a commit this clone does not have.
        let other = tempfile::tempdir().expect("tempdir");
        let url = r.origin.path().to_string_lossy().to_string();
        sh(other.path(), &["clone", "--quiet", &url, "."]);
        sh(
            other.path(),
            &["checkout", "--quiet", "-b", "qa", "origin/main"],
        );
        write(other.path(), "theirs.txt", "t\n");
        commit(other.path(), "theirs");
        sh(other.path(), &["push", "--quiet", "origin", "qa"]);
        let theirs = remote_sha(&r, "qa");
        let notes = r
            .git
            .land_release("main", "dev", "qa", "v1")
            .await
            .expect("land");
        assert_eq!(remote_sha(&r, "qa"), theirs, "never forced");
        assert!(notes.iter().any(|n| n.contains("qa")), "{notes:?}");
    }
}
