//! The environment variables the kernel process and its Python runtime (`runtime.py` and its modules) read, and the
//! interpreter the kernel runs.

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

/// Whether this process runs inside an Ultron kernel (a cell's command or a program it started): the kernel puts
/// its resource limits, `ULTRON_RLM_MAX_MEMORY_MB` among them, in every kernel's environment.
pub fn inside_kernel(var: impl Fn(&str) -> Option<String>) -> bool {
    var(RLM_MAX_MEMORY_MB_ENV).is_some()
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
