use std::collections::HashSet;
use std::path::{Path, PathBuf};

use aura_git::{GitError, GitService};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RepoWorkspaceInput {
    pub repo_id: Uuid,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub target_branch: String,
}

#[derive(Debug, Clone)]
pub struct RepoWorktree {
    pub repo_id: Uuid,
    pub repo_name: String,
    pub source_repo_path: PathBuf,
    pub worktree_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WorkspaceContainer {
    pub workspace_root: PathBuf,
    pub repo_worktrees: Vec<RepoWorktree>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("no repositories provided")]
    NoRepositories,
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("partial workspace creation failed: {0}")]
    PartialCreation(String),
}

pub struct WorkspaceManager<G: GitService> {
    git: G,
}

impl<G: GitService> WorkspaceManager<G> {
    pub fn new(git: G) -> Self {
        Self { git }
    }

    pub fn create_workspace(
        &self,
        workspace_root: &Path,
        repos: &[RepoWorkspaceInput],
        branch_name: &str,
    ) -> Result<WorkspaceContainer, WorkspaceError> {
        if repos.is_empty() {
            return Err(WorkspaceError::NoRepositories);
        }

        std::fs::create_dir_all(workspace_root)?;

        let mut created: Vec<RepoWorktree> = Vec::new();
        for repo in repos {
            let worktree_path = workspace_root.join(&repo.repo_name);
            let create_result = self.git.create_worktree(
                &repo.repo_path,
                &worktree_path,
                branch_name,
                &repo.target_branch,
                true,
            );

            if let Err(err) = create_result {
                for existing in &created {
                    let _ = self.git.remove_worktree(
                        &existing.source_repo_path,
                        &existing.worktree_path,
                        true,
                    );
                }
                let _ = std::fs::remove_dir_all(workspace_root);
                return Err(WorkspaceError::PartialCreation(err.to_string()));
            }

            created.push(RepoWorktree {
                repo_id: repo.repo_id,
                repo_name: repo.repo_name.clone(),
                source_repo_path: repo.repo_path.clone(),
                worktree_path,
            });
        }

        Ok(WorkspaceContainer {
            workspace_root: workspace_root.to_path_buf(),
            repo_worktrees: created,
        })
    }

    pub fn ensure_workspace_exists(
        &self,
        workspace_root: &Path,
        repos: &[RepoWorkspaceInput],
        branch_name: &str,
    ) -> Result<(), WorkspaceError> {
        if repos.is_empty() {
            return Err(WorkspaceError::NoRepositories);
        }

        std::fs::create_dir_all(workspace_root)?;

        for repo in repos {
            let worktree_path = workspace_root.join(&repo.repo_name);
            if worktree_path.exists() {
                continue;
            }
            self.git.create_worktree(
                &repo.repo_path,
                &worktree_path,
                branch_name,
                &repo.target_branch,
                false,
            )?;
        }

        Ok(())
    }

    pub fn cleanup_workspace(
        &self,
        workspace_root: &Path,
        repos: &[RepoWorkspaceInput],
    ) -> Result<(), WorkspaceError> {
        for repo in repos {
            let worktree_path = workspace_root.join(&repo.repo_name);
            if worktree_path.exists() {
                let _ = self
                    .git
                    .remove_worktree(&repo.repo_path, &worktree_path, true);
            }
        }

        if workspace_root.exists() {
            std::fs::remove_dir_all(workspace_root)?;
        }

        Ok(())
    }

    pub fn cleanup_orphan_workspaces(
        &self,
        workspaces_base: &Path,
        known_workspace_roots: &[PathBuf],
    ) -> Result<Vec<PathBuf>, WorkspaceError> {
        if !workspaces_base.exists() {
            return Ok(Vec::new());
        }

        let known = known_workspace_roots
            .iter()
            .cloned()
            .collect::<HashSet<PathBuf>>();
        let mut removed = Vec::new();

        for entry in std::fs::read_dir(workspaces_base)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && !known.contains(&path) {
                std::fs::remove_dir_all(&path)?;
                removed.push(path);
            }
        }

        Ok(removed)
    }

    pub fn git_branch_from_workspace(
        &self,
        workspace_id: Uuid,
        task_title: &str,
        branch_prefix: &str,
    ) -> String {
        let suffix = branch_suffix(workspace_id, task_title);
        if branch_prefix.trim().is_empty() {
            suffix
        } else {
            format!("{}/{}", branch_prefix, suffix)
        }
    }

    pub fn workspace_dir_name(&self, workspace_id: Uuid, task_title: &str) -> String {
        branch_suffix(workspace_id, task_title)
    }
}

fn branch_suffix(workspace_id: Uuid, task_title: &str) -> String {
    let short = workspace_id.as_simple().to_string()[..8].to_string();
    let slug = task_title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        short
    } else {
        format!("{}-{}", short, slug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_git::{DiffSummary, DiffTarget, GitResult, HeadInfo};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockGit {
        created: Arc<Mutex<Vec<PathBuf>>>,
        removed: Arc<Mutex<Vec<PathBuf>>>,
        fail_after: Option<usize>,
    }

    impl MockGit {
        fn with_fail_after(fail_after: usize) -> Self {
            Self {
                fail_after: Some(fail_after),
                ..Self::default()
            }
        }
    }

    impl GitService for MockGit {
        fn is_branch_name_valid(&self, _name: &str) -> bool {
            true
        }

        fn ensure_branch(
            &self,
            _repo_path: &Path,
            _branch_name: &str,
            _from_ref: &str,
        ) -> GitResult<()> {
            Ok(())
        }

        fn find_branch(&self, _repo_path: &Path, _branch_name: &str) -> GitResult<bool> {
            Ok(true)
        }

        fn stage_all(&self, _repo_path: &Path) -> GitResult<()> {
            Ok(())
        }

        fn commit_all(&self, _repo_path: &Path, _message: &str) -> GitResult<bool> {
            Ok(false)
        }

        fn has_changes(&self, _repo_path: &Path) -> GitResult<bool> {
            Ok(false)
        }

        fn head_info(&self, _repo_path: &Path) -> GitResult<HeadInfo> {
            Ok(HeadInfo {
                branch: "main".to_string(),
                oid: "abc".to_string(),
            })
        }

        fn merge_base(&self, _repo_path: &Path, _left: &str, _right: &str) -> GitResult<String> {
            Ok("abc".to_string())
        }

        fn diff_summary(&self, _target: DiffTarget<'_>) -> GitResult<DiffSummary> {
            Ok(DiffSummary::default())
        }

        fn create_worktree(
            &self,
            _repo_path: &Path,
            worktree_path: &Path,
            _branch_name: &str,
            _from_ref: &str,
            _create_branch: bool,
        ) -> GitResult<()> {
            let mut created = self.created.lock().expect("lock");
            created.push(worktree_path.to_path_buf());
            if let Some(fail_after) = self.fail_after
                && created.len() > fail_after
            {
                return Err(aura_git::GitError::Command("injected failure".to_string()));
            }
            Ok(())
        }

        fn remove_worktree(
            &self,
            _repo_path: &Path,
            worktree_path: &Path,
            _force: bool,
        ) -> GitResult<()> {
            self.removed
                .lock()
                .expect("lock")
                .push(worktree_path.to_path_buf());
            Ok(())
        }

        fn prune_worktrees(&self, _repo_path: &Path) -> GitResult<()> {
            Ok(())
        }

        fn detect_conflict_op(&self, _repo_path: &Path) -> GitResult<Option<aura_git::ConflictOp>> {
            Ok(None)
        }

        fn safe_merge(
            &self,
            _repo_path: &Path,
            _target_branch: &str,
            _source_ref: &str,
        ) -> GitResult<()> {
            Ok(())
        }
    }

    #[test]
    fn create_workspace_rolls_back_after_partial_failure() {
        let temp = tempfile::tempdir().expect("tmp");
        let manager = WorkspaceManager::new(MockGit::with_fail_after(1));

        let repos = vec![
            RepoWorkspaceInput {
                repo_id: Uuid::new_v4(),
                repo_name: "r1".to_string(),
                repo_path: temp.path().join("r1"),
                target_branch: "main".to_string(),
            },
            RepoWorkspaceInput {
                repo_id: Uuid::new_v4(),
                repo_name: "r2".to_string(),
                repo_path: temp.path().join("r2"),
                target_branch: "main".to_string(),
            },
        ];

        let err = manager
            .create_workspace(temp.path().join("ws").as_path(), &repos, "aura/test")
            .expect_err("must fail");
        assert!(matches!(err, WorkspaceError::PartialCreation(_)));
    }

    #[test]
    fn deterministic_branch_and_workspace_names() {
        let manager = WorkspaceManager::new(MockGit::default());
        let workspace_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").expect("uuid");
        let branch = manager.git_branch_from_workspace(workspace_id, "Ship API v2", "aura");
        assert!(branch.starts_with("aura/aaaaaaaa-ship-api-v2"));
        let dir = manager.workspace_dir_name(workspace_id, "Ship API v2");
        assert!(dir.starts_with("aaaaaaaa-ship-api-v2"));
    }
}
