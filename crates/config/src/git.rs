//! `findGitPaths` (core/footer-data-provider.ts, resource-loader.ts): the repository around a directory, as the
//! footer's branch watcher and the context files' worktree check read it.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

/// `findGitPaths`: walk up from `cwd` to the first `.git`: a directory (its `HEAD` must exist), or a worktree's
/// `gitdir: <dir>` file (its `HEAD` must exist; `commondir` names the shared git directory).
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git = dir.join(".git");
        if git.exists() {
            let meta = std::fs::metadata(&git).ok()?;
            if meta.is_file() {
                let content = std::fs::read_to_string(&git).ok()?;
                let content = content.trim();
                if let Some(target) = content.strip_prefix("gitdir: ") {
                    let git_dir = jsrt::path::absolute(&dir.join(target.trim()));
                    let head_path = git_dir.join("HEAD");
                    if !head_path.exists() {
                        return None;
                    }
                    let common = git_dir.join("commondir");
                    let common_git_dir = if common.exists() {
                        jsrt::path::absolute(&git_dir.join(std::fs::read_to_string(&common).ok()?.trim()))
                    } else {
                        git_dir.clone()
                    };
                    return Some(GitPaths {
                        repo_dir: dir,
                        common_git_dir,
                        head_path,
                    });
                }
            } else if meta.is_dir() {
                let head_path = git.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir: git,
                    head_path,
                });
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}
