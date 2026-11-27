//! `findGitPaths` (core/footer-data-provider.ts, resource-loader.ts): the repository around a directory, as the
//! footer's branch watcher and the context files' worktree check read it.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

enum GitProbe {
    Found(GitPaths),
    Stop,
    Continue,
}

fn probe_git_file(dir: &Path, git: &Path) -> GitProbe {
    let Ok(content) = std::fs::read_to_string(git) else {
        return GitProbe::Stop;
    };
    let Some(target) = content.trim().strip_prefix("gitdir: ") else {
        return GitProbe::Continue;
    };
    let git_dir = jsrt::path::absolute(&dir.join(target.trim()));
    let head_path = git_dir.join("HEAD");
    if !head_path.exists() {
        return GitProbe::Stop;
    }
    let common = git_dir.join("commondir");
    let common_git_dir = if common.exists() {
        let Ok(common_text) = std::fs::read_to_string(&common) else {
            return GitProbe::Stop;
        };
        jsrt::path::absolute(&git_dir.join(common_text.trim()))
    } else {
        git_dir
    };
    GitProbe::Found(GitPaths {
        repo_dir: dir.to_path_buf(),
        common_git_dir,
        head_path,
    })
}

fn probe_git_dir(dir: &Path, git: PathBuf) -> GitProbe {
    let head_path = git.join("HEAD");
    if !head_path.exists() {
        return GitProbe::Stop;
    }
    GitProbe::Found(GitPaths {
        repo_dir: dir.to_path_buf(),
        common_git_dir: git,
        head_path,
    })
}

fn probe_git_entry(dir: &Path) -> GitProbe {
    let git = dir.join(".git");
    if !git.exists() {
        return GitProbe::Continue;
    }
    let Ok(meta) = std::fs::metadata(&git) else {
        return GitProbe::Stop;
    };
    if meta.is_file() {
        probe_git_file(dir, &git)
    } else if meta.is_dir() {
        probe_git_dir(dir, git)
    } else {
        GitProbe::Continue
    }
}

/// `findGitPaths`: walk up from `cwd` to the first `.git`: a directory (its `HEAD` must exist), or a worktree's
/// `gitdir: <dir>` file (its `HEAD` must exist; `commondir` names the shared git directory).
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        match probe_git_entry(&dir) {
            GitProbe::Found(paths) => return Some(paths),
            GitProbe::Stop => return None,
            GitProbe::Continue => {
                if !dir.pop() {
                    return None;
                }
            }
        }
    }
}
