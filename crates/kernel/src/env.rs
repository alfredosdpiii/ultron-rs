//! The environment variables the kernel process and its Python runtime (`runtime.py` and its modules) read, where a
//! subagent's worktree lives, and the interpreter the kernel runs.

use std::path::Path;

pub const PYTHON_ENV: &str = "ULTRON_PYTHON";
pub const RLM_ENV_ALLOW_ENV: &str = "ULTRON_RLM_ENV_ALLOW";
pub const RLM_MAX_MEMORY_MB_ENV: &str = "ULTRON_RLM_MAX_MEMORY_MB";
pub const RLM_MAX_CPU_SECONDS_ENV: &str = "ULTRON_RLM_MAX_CPU_SECONDS";
pub const RLM_MAX_TREE_MEMORY_MB_ENV: &str = "ULTRON_RLM_MAX_TREE_MEMORY_MB";
pub const RLM_TREE_MEMORY_BACKEND_ENV: &str = "ULTRON_RLM_TREE_MEMORY_BACKEND";
pub const RLM_OUTPUT_BYTES_ENV: &str = "ULTRON_RLM_OUTPUT_BYTES";
pub const RLM_SPILL_DIR_ENV: &str = "ULTRON_RLM_SPILL_DIR";
pub const RLM_STREAM_OUTPUT_ENV: &str = "ULTRON_RLM_STREAM_OUTPUT";
pub const CODE_SKILLS_DIR_ENV: &str = "ULTRON_CODE_SKILLS_DIR";
pub const BASH_YIELD_AFTER_ENV: &str = "ULTRON_BASH_YIELD_AFTER";
pub const CELL_YIELD_AFTER_ENV: &str = "ULTRON_CELL_YIELD_AFTER";
pub const FILE_HOOKS_ENV: &str = "ULTRON_FILE_HOOKS";
/// A subagent's worktree, its parent's checkout and its branch (`worktree_api.py`).
pub const WORKTREE_ENV: &str = "ULTRON_WORKTREE";
pub const PARENT_REPO_ENV: &str = "ULTRON_PARENT_REPO";
pub const WORKTREE_BRANCH_ENV: &str = "ULTRON_WORKTREE_BRANCH";
/// The directory under the Git common dir that holds subagent worktrees and their records (worktrees.ts
/// `WORKTREES_DIR`).
pub const WORKTREES_DIR: &str = "ultron-worktrees";

/// Set (to `1`) in every kernel's environment, so a cell's command or a program it started knows it runs inside a
/// kernel. Rust-only; the resource limits beside it may also come from the user.
pub const IN_KERNEL_ENV: &str = "ULTRON_IN_KERNEL";

/// Whether this process runs inside an Ultron kernel ([`IN_KERNEL_ENV`]).
pub fn inside_kernel(var: impl Fn(&str) -> Option<String>) -> bool {
    var(IN_KERNEL_ENV).as_deref() == Some("1")
}

/// kernel.ts: the interpreter the kernel runs, `ULTRON_PYTHON ?? (linux && /usr/bin/python3 exists ?
/// "/usr/bin/python3" : "python3")`.
pub fn python(var: impl Fn(&str) -> Option<String>) -> String {
    var(PYTHON_ENV).unwrap_or_else(|| {
        if cfg!(target_os = "linux") && Path::new("/usr/bin/python3").exists() {
            "/usr/bin/python3".into()
        } else {
            "python3".into()
        }
    })
}
