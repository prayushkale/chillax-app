//! The version-control adapter. The dev loop creates a worktree and a
//! branch per ticket, commits and merges through this trait; `chillax-vcs` implements it
//! by shelling out to `git` (AGENTS.md rule 8).

use std::path::Path;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("git {command} failed: {detail}")]
    Git { command: String, detail: String },
    #[error("could not run git (is it installed?): {0}")]
    Io(String),
    #[error("git {0} timed out")]
    Timeout(String),
    /// The base branch is checked out somewhere Chillax must not touch.
    #[error("{0}")]
    BaseBusy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        commit: String,
    },
    /// The merge conflicted and was aborted; the base branch is unchanged. `files` are
    /// the conflicted paths, when git named them.
    Conflict {
        files: Vec<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseResult {
    Rebased,
    /// The rebase conflicted and was aborted; the branch is unchanged.
    Conflict {
        files: Vec<String>,
        detail: String,
    },
    /// Uncommitted changes in the worktree; nothing was done.
    Dirty,
}

/// What merging the base branch into a ticket's branch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncResult {
    /// The branch already has every commit of the base; nothing changed.
    UpToDate,
    /// The base was merged in with a new merge commit, so the tests must run again.
    Merged { commit: String },
    /// The merge conflicted and was aborted; the branch is unchanged.
    Conflict { files: Vec<String>, detail: String },
    /// Uncommitted changes in the worktree; nothing was done.
    Dirty,
}

/// Where an agent's conflict resolution stands in a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConflictStatus {
    /// A merge is still in progress (`MERGE_HEAD`) or paths are unmerged.
    pub merging: bool,
    /// The branch contains the base branch's head.
    pub base_merged: bool,
    /// Files that still hold conflict markers.
    pub markers: Vec<String>,
}

impl ConflictStatus {
    /// Nothing is left half-merged: no merge in progress and no markers in the files.
    pub fn clean(&self) -> bool {
        !self.merging && self.markers.is_empty()
    }

    /// The agent merged the base and finished the merge cleanly.
    pub fn resolved(&self) -> bool {
        self.clean() && self.base_merged
    }
}

/// What advancing a branch to another did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FastForward {
    /// The branch moved (or was created) at `commit`.
    Advanced { commit: String },
    /// The branch was already there.
    UpToDate { commit: String },
    /// The branch has commits the target lacks, so it cannot be fast-forwarded; it is
    /// unchanged.
    Diverged { detail: String },
}

/// A pull request on the code host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

#[async_trait]
pub trait VcsHost: Send + Sync {
    /// Create `branch` from the base branch and check it out at `path`. A branch or
    /// worktree left over from an earlier run is reused.
    async fn create_worktree(&self, branch: &str, path: &Path) -> Result<(), VcsError>;

    /// Check `branch` out in the existing worktree at `path` (`git switch`). A missing
    /// branch is created from `create_from`, which must then be given. Shared mode (ADR 0025)
    /// moves the one dev worktree between ticket branches and `dev` this way. Switching to
    /// the base branch while another checkout holds it leaves the worktree detached there.
    async fn switch_branch(
        &self,
        _path: &Path,
        _branch: &str,
        _create_from: Option<&str>,
    ) -> Result<(), VcsError> {
        Err(VcsError::Io(
            "this version-control adapter cannot switch branches".into(),
        ))
    }

    /// Remove a worktree and its directory. The branch stays.
    async fn remove_worktree(&self, path: &Path) -> Result<(), VcsError>;

    /// Commit everything in the worktree. `None` when there was nothing to commit.
    async fn commit_all(&self, path: &Path, message: &str) -> Result<Option<String>, VcsError>;

    async fn is_clean(&self, path: &Path) -> Result<bool, VcsError>;

    /// What `branch` adds to the base branch (`base...branch`), cut at `max_bytes`.
    async fn diff(&self, branch: &str, max_bytes: usize) -> Result<String, VcsError>;

    /// Merge `branch` into the base branch with a merge commit.
    async fn merge(&self, branch: &str, message: &str) -> Result<MergeOutcome, VcsError>;

    /// Rebase the worktree's branch onto the base branch.
    async fn rebase(&self, path: &Path) -> Result<RebaseResult, VcsError>;

    /// Merge the base branch into the branch checked out at `path` (a ticket's branch), so
    /// it is tested against what it will merge into. A conflict is aborted.
    async fn sync_with_base(&self, _path: &Path, _message: &str) -> Result<SyncResult, VcsError> {
        Err(VcsError::Io(
            "this version-control adapter cannot sync branches".into(),
        ))
    }

    /// How far a conflict resolution in `path` got; `files` are the paths that conflicted,
    /// searched for leftover markers.
    async fn conflict_status(
        &self,
        _path: &Path,
        _files: &[String],
    ) -> Result<ConflictStatus, VcsError> {
        Err(VcsError::Io(
            "this version-control adapter cannot inspect conflicts".into(),
        ))
    }

    /// Move `branch` forward to `to` without a merge commit, creating `branch` at `to`
    /// when it does not exist yet. Promotion of `dev` to `qa` is this.
    async fn fast_forward(&self, branch: &str, to: &str) -> Result<FastForward, VcsError>;

    /// Merge `source` into `target` with a merge commit, like [`VcsHost::merge`] does
    /// for the base branch. A release is `qa` merged into the prod branch.
    async fn merge_into(
        &self,
        target: &str,
        source: &str,
        message: &str,
    ) -> Result<MergeOutcome, VcsError>;

    /// Commit a file (a path inside the repository) to `branch` and return the new
    /// head. The changelog is committed to `qa` this way.
    async fn commit_file(
        &self,
        branch: &str,
        path: &str,
        contents: &str,
        message: &str,
    ) -> Result<String, VcsError>;

    /// Create an annotated tag at `commit`. An existing tag is an error: a release is
    /// never re-tagged.
    async fn tag(&self, name: &str, commit: &str, message: &str) -> Result<(), VcsError>;

    /// The commit `branch` points at.
    async fn head(&self, branch: &str) -> Result<String, VcsError>;

    /// `path` as committed on `branch`; `None` when the branch or the file is missing.
    async fn read_file(&self, branch: &str, path: &str) -> Result<Option<String>, VcsError>;

    /// The commit a tag points at, if the tag exists. A release interrupted after
    /// tagging finds its tag again instead of tagging twice.
    async fn tag_commit(&self, name: &str) -> Result<Option<String>, VcsError>;

    /// The directory where `branch` is checked out: Chillax's own worktree for it, the
    /// place the stakeholder already has it checked out, or a new worktree. QA tests
    /// and prod deploys run there.
    async fn checkout(&self, branch: &str) -> Result<std::path::PathBuf, VcsError>;

    /// A worktree with `commit` checked out detached, created or moved there. Prod apps
    /// run from here at the release tag (ADR 0015). The name `prod` is the main worktree
    /// (`_main`), shared with releases (ADR 0025); any other name gets `_<name>`.
    /// Untracked files are kept, so installed dependencies survive a move.
    async fn detached_checkout(
        &self,
        _name: &str,
        _commit: &str,
    ) -> Result<std::path::PathBuf, VcsError> {
        Err(VcsError::Io(
            "this version-control adapter cannot make detached checkouts".into(),
        ))
    }

    // --- Remote operations. Every one defaults to "nothing to do", which is
    // right for a purely local repository; a code-host adapter overrides them. ---

    /// Whether work travels as pull requests on a remote. The dev loop and the release
    /// take the pull-request path when this is true.
    fn uses_pull_requests(&self) -> bool {
        false
    }

    /// Push `branch` to the remote. `force_with_lease` is for branches Chillax rebases.
    async fn push(&self, _branch: &str, _force_with_lease: bool) -> Result<(), VcsError> {
        Ok(())
    }

    /// Open a pull request of `head` into `base`, or return the one already open for that
    /// pair. `None` when nothing is left to merge (the base already has the branch) or
    /// there is no remote.
    async fn open_pr(
        &self,
        _head: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<Option<PullRequest>, VcsError> {
        Ok(None)
    }

    async fn comment_pr(&self, _number: u64, _body: &str) -> Result<(), VcsError> {
        Ok(())
    }

    /// Merge pull request `number` into `base` with a merge commit; the commit is the new
    /// head of `base` on the remote.
    async fn merge_pr(
        &self,
        _number: u64,
        _base: &str,
        _message: &str,
    ) -> Result<MergeOutcome, VcsError> {
        Err(VcsError::Io("this repository has no remote".into()))
    }

    async fn push_tag(&self, _name: &str) -> Result<(), VcsError> {
        Ok(())
    }

    /// Publish a release for `tag` with `notes`; returns its URL.
    async fn create_release(
        &self,
        _tag: &str,
        _title: &str,
        _notes: &str,
    ) -> Result<Option<String>, VcsError> {
        Ok(None)
    }

    async fn fetch(&self) -> Result<(), VcsError> {
        Ok(())
    }

    /// Fetch, then fast-forward the local `branch` to the remote's, but only where that
    /// cannot disturb anyone: a checkout with uncommitted changes is left alone. Returns a
    /// note when the local branch was left behind.
    async fn sync_local(&self, _branch: &str) -> Result<Option<String>, VcsError> {
        Ok(None)
    }

    /// The commit the remote has for `branch`, as of the last fetch.
    async fn remote_head(&self, _branch: &str) -> Result<Option<String>, VcsError> {
        Ok(None)
    }

    /// After a deployed release: bring the base, `qa` and prod branches to one commit
    /// (fast-forward only), push them and `tag`, and check the prod branch out in the
    /// primary checkout. Returns a note for everything skipped; never forces.
    async fn land_release(
        &self,
        _prod: &str,
        _base: &str,
        _qa: &str,
        _tag: &str,
    ) -> Result<Vec<String>, VcsError> {
        Ok(Vec::new())
    }

    /// What would make a release fail, found before anything is changed. Empty when it
    /// would go through.
    async fn release_preflight(
        &self,
        _qa: &str,
        _prod: &str,
        _base: &str,
    ) -> Result<Vec<String>, VcsError> {
        Ok(Vec::new())
    }
}
