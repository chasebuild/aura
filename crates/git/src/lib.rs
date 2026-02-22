use std::path::Path;
use std::process::Command;

use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("branch not found: {0}")]
    BranchNotFound(String),
    #[error("worktree is dirty: {0}")]
    WorktreeDirty(String),
    #[error("conflict operation in progress: {0:?}")]
    ConflictInProgress(ConflictOp),
}

pub type GitResult<T> = Result<T, GitError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadInfo {
    pub branch: String,
    pub oid: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WorktreeResetOptions {
    pub perform_reset: bool,
    pub force_when_dirty: bool,
    pub is_dirty: bool,
    pub log_skip_when_dirty: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeResetOutcome {
    pub needed: bool,
    pub applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictOp {
    Rebase,
    Merge,
    CherryPick,
    Revert,
}

#[derive(Debug, Clone)]
pub enum DiffTarget<'a> {
    Worktree {
        repo_path: &'a Path,
    },
    Branch {
        repo_path: &'a Path,
        base_branch: &'a str,
        head_branch: &'a str,
    },
    Commit {
        repo_path: &'a Path,
        commit_sha: &'a str,
        base_branch: Option<&'a str>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiffFileStat {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiffSummary {
    pub files: Vec<DiffFileStat>,
}

pub trait GitService: Send + Sync {
    fn is_branch_name_valid(&self, name: &str) -> bool;
    fn ensure_branch(&self, repo_path: &Path, branch_name: &str, from_ref: &str) -> GitResult<()>;
    fn find_branch(&self, repo_path: &Path, branch_name: &str) -> GitResult<bool>;
    fn stage_all(&self, repo_path: &Path) -> GitResult<()>;
    fn commit_all(&self, repo_path: &Path, message: &str) -> GitResult<bool>;
    fn has_changes(&self, repo_path: &Path) -> GitResult<bool>;
    fn head_info(&self, repo_path: &Path) -> GitResult<HeadInfo>;
    fn merge_base(&self, repo_path: &Path, left: &str, right: &str) -> GitResult<String>;
    fn diff_summary(&self, target: DiffTarget<'_>) -> GitResult<DiffSummary>;
    fn create_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        branch_name: &str,
        from_ref: &str,
        create_branch: bool,
    ) -> GitResult<()>;
    fn remove_worktree(&self, repo_path: &Path, worktree_path: &Path, force: bool)
    -> GitResult<()>;
    fn prune_worktrees(&self, repo_path: &Path) -> GitResult<()>;
    fn detect_conflict_op(&self, repo_path: &Path) -> GitResult<Option<ConflictOp>>;
    fn safe_merge(&self, repo_path: &Path, target_branch: &str, source_ref: &str) -> GitResult<()>;
}

#[derive(Debug, Clone, Default)]
pub struct GitServiceImpl;

impl GitServiceImpl {
    pub fn new() -> Self {
        Self
    }

    fn open_repo(&self, repo_path: &Path) -> GitResult<Repository> {
        Ok(Repository::open(repo_path)?)
    }

    fn git(&self, repo_path: &Path, args: &[&str]) -> GitResult<String> {
        let output = Command::new("git")
            .args(["-C", repo_path.to_string_lossy().as_ref()])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GitError::Command(stderr.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn ensure_signature(&self, repo: &Repository) -> GitResult<git2::Signature<'_>> {
        match repo.signature() {
            Ok(sig) => Ok(sig),
            Err(_) => Ok(git2::Signature::now("Aura", "noreply@aura.local")?),
        }
    }
}

impl GitService for GitServiceImpl {
    fn is_branch_name_valid(&self, name: &str) -> bool {
        git2::Branch::name_is_valid(name).unwrap_or(false)
    }

    fn ensure_branch(&self, repo_path: &Path, branch_name: &str, from_ref: &str) -> GitResult<()> {
        let repo = self.open_repo(repo_path)?;
        if repo.find_branch(branch_name, BranchType::Local).is_ok() {
            return Ok(());
        }

        let base_obj = repo.revparse_single(from_ref)?;
        let base_commit = base_obj.peel_to_commit()?;
        repo.branch(branch_name, &base_commit, false)?;
        Ok(())
    }

    fn find_branch(&self, repo_path: &Path, branch_name: &str) -> GitResult<bool> {
        let repo = self.open_repo(repo_path)?;
        Ok(repo.find_branch(branch_name, BranchType::Local).is_ok())
    }

    fn stage_all(&self, repo_path: &Path) -> GitResult<()> {
        let _ = self.git(repo_path, &["add", "-A"])?;
        Ok(())
    }

    fn commit_all(&self, repo_path: &Path, message: &str) -> GitResult<bool> {
        if !self.has_changes(repo_path)? {
            return Ok(false);
        }

        self.stage_all(repo_path)?;

        let repo = self.open_repo(repo_path)?;
        let signature = self.ensure_signature(&repo)?;

        let tree_id = {
            let mut index = repo.index()?;
            index.write_tree()?
        };
        let tree = repo.find_tree(tree_id)?;

        let parent_commit = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok());

        if let Some(parent) = parent_commit {
            let _ = repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )?;
        } else {
            let _ = repo.commit(Some("HEAD"), &signature, &signature, message, &tree, &[])?;
        }

        Ok(true)
    }

    fn has_changes(&self, repo_path: &Path) -> GitResult<bool> {
        let output = self.git(repo_path, &["status", "--porcelain"])?;
        Ok(!output.trim().is_empty())
    }

    fn head_info(&self, repo_path: &Path) -> GitResult<HeadInfo> {
        let repo = self.open_repo(repo_path)?;
        let head = repo.head()?;
        let branch = head.shorthand().unwrap_or("HEAD").to_string();
        let oid = head
            .target()
            .ok_or_else(|| GitError::Command("HEAD has no target".to_string()))?
            .to_string();
        Ok(HeadInfo { branch, oid })
    }

    fn merge_base(&self, repo_path: &Path, left: &str, right: &str) -> GitResult<String> {
        let repo = self.open_repo(repo_path)?;
        let left_oid = repo.revparse_single(left)?.id();
        let right_oid = repo.revparse_single(right)?.id();
        let base = repo.merge_base(left_oid, right_oid)?;
        Ok(base.to_string())
    }

    fn diff_summary(&self, target: DiffTarget<'_>) -> GitResult<DiffSummary> {
        let out = match target {
            DiffTarget::Worktree { repo_path } => self.git(repo_path, &["diff", "--numstat"])?,
            DiffTarget::Branch {
                repo_path,
                base_branch,
                head_branch,
            } => {
                let range = format!("{base_branch}..{head_branch}");
                self.git(repo_path, &["diff", "--numstat", &range])?
            }
            DiffTarget::Commit {
                repo_path,
                commit_sha,
                base_branch,
            } => {
                let range = if let Some(base) = base_branch {
                    format!("{base}..{commit_sha}")
                } else {
                    format!("{commit_sha}^..{commit_sha}")
                };
                self.git(repo_path, &["diff", "--numstat", &range])?
            }
        };
        let files = out
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let add = parts.next()?.parse::<usize>().ok()?;
                let rem = parts.next()?.parse::<usize>().ok()?;
                let path = parts.next()?.to_string();
                Some(DiffFileStat {
                    path,
                    added: add,
                    removed: rem,
                })
            })
            .collect::<Vec<_>>();

        Ok(DiffSummary { files })
    }

    fn create_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        branch_name: &str,
        from_ref: &str,
        create_branch: bool,
    ) -> GitResult<()> {
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if create_branch {
            self.ensure_branch(repo_path, branch_name, from_ref)?;
        }

        let worktree = worktree_path.to_string_lossy().to_string();
        let args = ["worktree", "add", "--force", &worktree, branch_name];
        let _ = self.git(repo_path, &args)?;
        Ok(())
    }

    fn remove_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        force: bool,
    ) -> GitResult<()> {
        let worktree = worktree_path.to_string_lossy().to_string();
        if force {
            let _ = self.git(repo_path, &["worktree", "remove", "--force", &worktree])?;
        } else {
            let _ = self.git(repo_path, &["worktree", "remove", &worktree])?;
        }
        Ok(())
    }

    fn prune_worktrees(&self, repo_path: &Path) -> GitResult<()> {
        let _ = self.git(repo_path, &["worktree", "prune"])?;
        Ok(())
    }

    fn detect_conflict_op(&self, repo_path: &Path) -> GitResult<Option<ConflictOp>> {
        let repo = self.open_repo(repo_path)?;
        let git_dir = repo.path();

        if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            return Ok(Some(ConflictOp::Rebase));
        }
        if git_dir.join("MERGE_HEAD").exists() {
            return Ok(Some(ConflictOp::Merge));
        }
        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            return Ok(Some(ConflictOp::CherryPick));
        }
        if git_dir.join("REVERT_HEAD").exists() {
            return Ok(Some(ConflictOp::Revert));
        }

        Ok(None)
    }

    fn safe_merge(&self, repo_path: &Path, target_branch: &str, source_ref: &str) -> GitResult<()> {
        if let Some(op) = self.detect_conflict_op(repo_path)? {
            return Err(GitError::ConflictInProgress(op));
        }

        if self.has_changes(repo_path)? {
            return Err(GitError::WorktreeDirty(format!(
                "{} has uncommitted changes",
                repo_path.display()
            )));
        }

        let _ = self.git(repo_path, &["checkout", target_branch])?;
        let output = Command::new("git")
            .args([
                "-C",
                repo_path.to_string_lossy().as_ref(),
                "merge",
                "--no-ff",
                "--no-edit",
                source_ref,
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("conflict") {
            return Err(GitError::Command("merge conflicts detected".to_string()));
        }

        Err(GitError::Command(stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-C", repo.to_string_lossy().as_ref()])
            .args(args)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tmp");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "aura@example.com"]);
        git(dir.path(), &["config", "user.name", "Aura"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "init"]);
        dir
    }

    #[test]
    fn validates_branch_names() {
        let svc = GitServiceImpl::new();
        assert!(svc.is_branch_name_valid("feature/my-work"));
        assert!(!svc.is_branch_name_valid("foo..bar"));
    }

    #[test]
    fn safe_merge_refuses_dirty_tree() {
        let svc = GitServiceImpl::new();
        let repo = init_repo();

        std::fs::write(repo.path().join("README.md"), "dirty\n").expect("write");

        let err = svc
            .safe_merge(repo.path(), "main", "main")
            .expect_err("should fail for dirty tree");
        assert!(matches!(err, GitError::WorktreeDirty(_)));
    }

    #[test]
    fn ensure_branch_creates_new_branch() {
        let svc = GitServiceImpl::new();
        let repo = init_repo();

        svc.ensure_branch(repo.path(), "feature/a", "main")
            .expect("branch create");
        assert!(
            svc.find_branch(repo.path(), "feature/a")
                .expect("find branch")
        );
    }
}
