//! Ephemeral git worktrees for isolated agents. Creation is ASYNC and happens
//! in the runner AFTER a concurrency permit is held (codex M6 review #1): a
//! 300-agent isolate fan-out creates at most `concurrency` worktrees at a
//! time, and an agent refused by the post-queue budget check never creates
//! one. Removal is a sync best-effort Drop that also cleans the source repo's
//! worktree bookkeeping.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the engine knows at spawn time; the runner materializes it once the
/// agent actually starts.
#[derive(Debug, Clone)]
pub struct WorktreeSpec {
    /// Repo to fork from (the workflow file's directory).
    pub repo: PathBuf,
    /// Base dir for worktrees — the run tempdir. The Arc keeps it alive so a
    /// run teardown can't delete it under a live guard (codex M6 review #2).
    pub base: Arc<tempfile::TempDir>,
}

/// RAII worktree: `git worktree add --detach` on create; Drop runs
/// `git worktree remove --force` (directory + repo bookkeeping) on every path.
/// Ephemeral by design — a workflow must extract what it wants (diff, files)
/// before the agent returns.
pub struct WorktreeGuard {
    repo: PathBuf,
    path: PathBuf,
    _base: Arc<tempfile::TempDir>,
}

impl WorktreeGuard {
    pub async fn add(spec: &WorktreeSpec, id: u64) -> Result<Self, String> {
        let path = spec.base.path().join(format!("wt-{id}"));
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&spec.repo)
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .output()
            .await
            .map_err(|e| format!("isolate: cannot run git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "isolate: git worktree add failed (is {} inside a git repo?): {}",
                spec.repo.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(Self {
            repo: spec.repo.clone(),
            path,
            _base: spec.base.clone(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn add_then_drop_removes_dir_and_bookkeeping() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("f"), "x").unwrap();
        git(repo.path(), &["add", "."]);
        git(
            repo.path(),
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "i"],
        );

        let spec = WorktreeSpec {
            repo: repo.path().to_path_buf(),
            base: Arc::new(tempfile::tempdir().unwrap()),
        };
        let g = block_on(WorktreeGuard::add(&spec, 7)).unwrap();
        let wt = g.path().to_path_buf();
        assert!(wt.join("f").exists(), "worktree has the repo content");
        drop(g);
        assert!(!wt.exists(), "directory removed on drop");
        let list = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&list.stdout).lines().count(),
            1,
            "repo bookkeeping cleaned (main worktree only)"
        );
    }

    #[test]
    fn non_repo_is_an_error_not_a_panic() {
        let not_repo = tempfile::tempdir().unwrap();
        let spec = WorktreeSpec {
            repo: not_repo.path().to_path_buf(),
            base: Arc::new(tempfile::tempdir().unwrap()),
        };
        assert!(block_on(WorktreeGuard::add(&spec, 1)).is_err());
    }
}
