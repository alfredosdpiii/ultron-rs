//! AGENTS.md / CLAUDE.md discovery (`loadProjectContextFiles` in core/resource-loader.ts).

use crate::paths::{canonicalize, resolve_path, strip_bom};
use jsrt::path::{basename, dirname};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

const CANDIDATES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

fn load_from_dir(dir: &str) -> Option<ContextFile> {
    CANDIDATES.iter().find_map(|name| {
        let path = jsrt::path::join(&[dir, name]);
        if !fs::metadata(&path).ok()?.is_file() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        Some(ContextFile {
            content: strip_bom(&content).to_string(),
            path,
        })
    })
}

/// The main repo's context file that a nested linked worktree's own copy shadows (canonical path).
fn find_shadowed(cwd: &str) -> Option<String> {
    let git = config::git::find_git_paths(Path::new(cwd))?;
    let common = canonicalize(&git.common_git_dir.to_string_lossy());
    let worktree_root = canonicalize(&git.repo_dir.to_string_lossy());
    let main_root = dirname(&common);
    if !worktree_root.starts_with(&format!("{main_root}/")) {
        return None;
    }
    if canonicalize(&jsrt::path::join(&[&main_root, ".git"])) != common {
        return None;
    }
    let own = load_from_dir(&worktree_root)?;
    Some(jsrt::path::join(&[&main_root, basename(&own.path)]))
}

/// The global context file from `agent_dir`, then one per ancestor of `cwd`, outermost first.
pub fn load_project_context_files(cwd: &str, agent_dir: &str) -> Vec<ContextFile> {
    let cwd = resolve_path(cwd, "/", false);
    let agent_dir = resolve_path(agent_dir, &cwd, false);
    let mut files = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(global) = load_from_dir(&agent_dir) {
        seen.push(global.path.clone());
        files.push(global);
    }
    let shadowed = find_shadowed(&cwd);
    let mut ancestors = Vec::new();
    let mut dir = cwd;
    loop {
        if let Some(file) = load_from_dir(&dir) {
            let is_shadowed = shadowed.as_ref().is_some_and(|s| &canonicalize(&file.path) == s);
            if !is_shadowed && !seen.contains(&file.path) {
                seen.push(file.path.clone());
                ancestors.insert(0, file);
            }
        }
        let parent = dirname(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }
    files.extend(ancestors);
    files
}
