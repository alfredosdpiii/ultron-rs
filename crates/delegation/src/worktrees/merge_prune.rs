use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictHunk {
    /// First line of the conflict in the merged file (1-based).
    pub line: usize,
    pub ours: String,
    pub theirs: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    pub kind: String,
    pub hunks: Vec<ConflictHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        files: Vec<String>,
        diffstat: String,
    },
    Empty,
    Conflict {
        files: Vec<String>,
        conflicts: Vec<ConflictFile>,
        /// Conflict markers were written into the working tree.
        markers: bool,
        message: String,
    },
}

impl MergeOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            MergeOutcome::Merged { .. } => "merged",
            MergeOutcome::Empty => "empty",
            MergeOutcome::Conflict { .. } => "conflict",
        }
    }
}

#[derive(Debug, Clone)]
struct TreeChange {
    src_mode: String,
    dst_mode: String,
    src_oid: String,
    path: String,
}

fn tree_changes(top: &Path, from: &str, to: &str) -> Result<Vec<TreeChange>> {
    let raw = git_ok(top, &["diff-tree", "-r", "-z", "--no-renames", from, to])?;
    let parts: Vec<&str> = raw.split('\0').collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while index + 1 < parts.len() {
        let meta = parts[index];
        let path = parts[index + 1];
        let Some(meta) = meta.strip_prefix(':') else {
            break;
        };
        let fields: Vec<&str> = meta.split(' ').collect();
        changes.push(TreeChange {
            src_mode: fields.first().copied().unwrap_or("").into(),
            dst_mode: fields.get(1).copied().unwrap_or("").into(),
            src_oid: fields.get(2).copied().unwrap_or("").into(),
            path: path.into(),
        });
        index += 2;
    }
    Ok(changes)
}

fn is_symlink_drifted(top: &Path, change: &TreeChange, info: &std::fs::Metadata) -> Result<bool> {
    if !info.file_type().is_symlink() {
        return Ok(true);
    }
    let target = std::fs::read_link(top.join(&change.path))?;
    let oid = git_ok_with(
        top,
        &["hash-object", "--stdin"],
        GitOptions {
            env: Vec::new(),
            input: Some(target.as_os_str().as_encoded_bytes().to_vec()),
        },
    )?;
    Ok(oid.trim() != change.src_oid)
}

fn find_drifted_regular_files(top: &Path, files: &[&TreeChange]) -> Result<Vec<String>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let input = files.iter().map(|c| format!("{}\n", c.path)).collect::<String>();
    let oids = git_ok_with(
        top,
        &["hash-object", "--stdin-paths"],
        GitOptions {
            env: Vec::new(),
            input: Some(input.into_bytes()),
        },
    )?;
    let oids: Vec<&str> = oids.trim().split('\n').collect();
    let mut moved = Vec::new();
    for (index, change) in files.iter().enumerate() {
        if oids.get(index).copied() != Some(change.src_oid.as_str()) {
            moved.push(change.path.clone());
        }
    }
    Ok(moved)
}

/// Paths whose working-tree content no longer matches `changes`' source side.
fn drifted(top: &Path, changes: &[TreeChange]) -> Result<Vec<String>> {
    let mut moved = Vec::new();
    let mut files = Vec::new();
    for change in changes {
        if change.src_mode == "160000" || change.dst_mode == "160000" {
            continue;
        }
        let info = std::fs::symlink_metadata(top.join(&change.path)).ok();
        if change.src_mode == ZERO_MODE {
            if info.is_some() {
                moved.push(change.path.clone());
            }
            continue;
        }
        let Some(info) = info else {
            moved.push(change.path.clone());
            continue;
        };
        if change.src_mode == "120000" {
            if is_symlink_drifted(top, change, &info)? {
                moved.push(change.path.clone());
            }
            continue;
        }
        files.push(change);
    }
    moved.extend(find_drifted_regular_files(top, &files)?);
    Ok(moved)
}

fn checkout_written_changes(top: &Path, tree: &str, written: &[&TreeChange]) -> Result<()> {
    if written.is_empty() {
        return Ok(());
    }
    let temporary = TempIndex(temporary_index("merge-index"));
    let index = text(&temporary.0);
    let env = [("GIT_INDEX_FILE", index.as_str())];
    git_ok_with(top, &["read-tree", tree], opts(&env))?;
    for change in written {
        let target = top.join(&change.path);
        if std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::remove_dir_all(&target);
        }
    }
    let mut input = written
        .iter()
        .map(|c| c.path.as_str())
        .collect::<Vec<_>>()
        .join("\0")
        .into_bytes();
    input.push(0);
    git_ok_with(
        top,
        &quiet(&["checkout-index", "-f", "-z", "--stdin"]),
        GitOptions {
            env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            input: Some(input),
        },
    )?;
    Ok(())
}

fn remove_deleted_changes(top: &Path, changes: &[TreeChange]) {
    for change in changes.iter().filter(|c| c.dst_mode == ZERO_MODE) {
        let target = top.join(&change.path);
        let _ = std::fs::remove_file(&target);
        let mut dir = target.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            if current == top || !current.starts_with(top) || std::fs::remove_dir(&current).is_err() {
                break;
            }
            dir = current.parent().map(Path::to_path_buf);
        }
    }
}

/// Write `tree`'s side of `changes` into the working tree through a temporary index (the real index is untouched).
fn write_changes(top: &Path, tree: &str, changes: &[TreeChange]) -> Result<()> {
    let written: Vec<&TreeChange> = changes
        .iter()
        .filter(|c| c.dst_mode != ZERO_MODE && c.dst_mode != "160000")
        .collect();
    checkout_written_changes(top, tree, &written)?;
    remove_deleted_changes(top, changes);
    Ok(())
}

fn clip(text: &str) -> String {
    if utf16::len(text) > HUNK_TEXT_CHARS {
        format!("{}…", utf16::head(text, HUNK_TEXT_CHARS - 1))
    } else {
        text.to_string()
    }
}

/// The conflict regions of a merged file with markers.
pub fn conflict_hunks(text: &str) -> Vec<ConflictHunk> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() && hunks.len() < MAX_HUNKS_PER_FILE {
        if !lines[index].starts_with("<<<<<<< ") {
            index += 1;
            continue;
        }
        let start = index;
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        let mut side = 0;
        index += 1;
        while index < lines.len() {
            let line = lines[index];
            if line.starts_with("||||||| ") {
                side = 1;
            } else if line == "=======" {
                side = 2;
            } else if line.starts_with(">>>>>>> ") {
                break;
            } else if side == 0 {
                ours.push(line);
            } else if side == 2 {
                theirs.push(line);
            }
            index += 1;
        }
        hunks.push(ConflictHunk {
            line: start + 1,
            ours: clip(&ours.join("\n")),
            theirs: clip(&theirs.join("\n")),
        });
        index += 1;
    }
    hunks
}

struct MergeTree {
    tree: String,
    paths: Vec<String>,
    messages: Vec<(Vec<String>, String, String)>,
}

/// Parse `git merge-tree --write-tree -z --name-only` output.
fn parse_merge_tree(stdout: &str) -> MergeTree {
    let parts: Vec<&str> = stdout.split('\0').collect();
    let tree = parts.first().copied().unwrap_or("").trim().to_string();
    let mut index = 1;
    let mut paths: Vec<String> = Vec::new();
    while index < parts.len() && !parts[index].is_empty() {
        if !paths.iter().any(|p| p == parts[index]) {
            paths.push(parts[index].to_string());
        }
        index += 1;
    }
    index += 1;
    let mut messages = Vec::new();
    while index < parts.len() {
        let Ok(count) = parts[index].parse::<usize>() else {
            break;
        };
        let own: Vec<String> = parts
            .iter()
            .skip(index + 1)
            .take(count)
            .map(std::string::ToString::to_string)
            .collect();
        let kind = parts.get(index + 1 + count).copied().unwrap_or("").to_string();
        let text = parts.get(index + 2 + count).copied().unwrap_or("").trim().to_string();
        messages.push((own, kind, text));
        index += 3 + count;
    }
    MergeTree { tree, paths, messages }
}

/// What `rlm.merge` merges from a child.
#[derive(Debug, Clone)]
pub struct MergeSource {
    pub name: String,
    pub branch: String,
    pub base: String,
    pub commit: Option<String>,
}

fn drift_conflict(moved: Vec<String>) -> MergeOutcome {
    MergeOutcome::Conflict {
        conflicts: moved
            .iter()
            .map(|path| ConflictFile {
                path: path.clone(),
                kind: "CONFLICT (changed during the merge)".into(),
                hunks: Vec::new(),
            })
            .collect(),
        message: format!("files changed in the working tree while merging: {}", moved.join(", ")),
        files: moved,
        markers: false,
    }
}

fn merge_tree_error(code: i32, stderr: &str) -> WorktreeError {
    let lower = stderr.to_lowercase();
    if lower.contains("unknown option") || lower.contains("usage: git merge-tree") {
        return WorktreeError::new(
            "rlm.merge needs Git 2.40 or newer (git merge-tree --write-tree --merge-base); update Git",
        );
    }
    let why = first_lines(stderr, 3);
    WorktreeError::new(format!(
        "git merge-tree failed: {}",
        if why.is_empty() { format!("exit {code}") } else { why }
    ))
}

fn collect_conflict_files(top: &Path, parsed: &MergeTree) -> Result<(Vec<ConflictFile>, String)> {
    let mut conflicts = Vec::new();
    for path in parsed.paths.iter().take(MAX_CONFLICT_FILES) {
        let blob = git(top, &["cat-file", "blob", &format!("{}:{path}", parsed.tree)])?;
        let kind = parsed
            .messages
            .iter()
            .find(|(paths, kind, _)| paths.contains(path) && kind.starts_with("CONFLICT"))
            .map_or("CONFLICT".to_string(), |(_, kind, _)| kind.clone());
        conflicts.push(ConflictFile {
            path: path.clone(),
            kind,
            hunks: if blob.code == 0 {
                conflict_hunks(&blob.stdout)
            } else {
                Vec::new()
            },
        });
    }
    let message = parsed
        .messages
        .iter()
        .filter(|(_, kind, _)| kind.starts_with("CONFLICT"))
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((conflicts, message))
}

fn relabel_conflict_markers(top: &Path, paths: &[String], ours: &str, commit: &str, branch: &str) -> Result<()> {
    for path in paths {
        let file = top.join(path);
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let relabeled = content
            .replace(&format!("<<<<<<< {ours}"), "<<<<<<< working tree")
            .replace(&format!(">>>>>>> {commit}"), &format!(">>>>>>> {branch}"));
        std::fs::write(&file, relabeled)?;
    }
    Ok(())
}

fn finish_conflicted_merge(
    top: &Path,
    ours: &str,
    commit: &str,
    branch: &str,
    parsed: MergeTree,
    changes: &[TreeChange],
    markers: bool,
) -> Result<MergeOutcome> {
    let (conflicts, message) = collect_conflict_files(top, &parsed)?;
    if !markers {
        return Ok(MergeOutcome::Conflict {
            files: parsed.paths,
            conflicts,
            markers: false,
            message,
        });
    }
    let moved = drifted(top, changes)?;
    if !moved.is_empty() {
        return Ok(drift_conflict(moved));
    }
    write_changes(top, &parsed.tree, changes)?;
    relabel_conflict_markers(top, &parsed.paths, ours, commit, branch)?;
    Ok(MergeOutcome::Conflict {
        files: parsed.paths,
        conflicts,
        markers: true,
        message,
    })
}

fn prepare_merge_target(target_cwd: &Path, source: &MergeSource, commit: &str) -> Result<(PathBuf, String)> {
    let target = repo_info(target_cwd)?;
    let Some(head) = target.head.as_deref() else {
        return Err(WorktreeError::new(format!(
            "the repository at {} has no commits to merge into",
            text(&target.top)
        )));
    };
    let top = target.top;
    if git(&top, &["cat-file", "-e", &format!("{commit}^{{commit}}")])?.code != 0 {
        return Err(WorktreeError::new(format!(
            "{}'s commit {} is not in the repository at {}",
            source.name,
            &commit[..commit.len().min(12)],
            text(&top)
        )));
    }
    let (ours, _) = snapshot_working_tree(
        &top,
        head,
        &format!("ultron: working tree before merging sub-agent {}", source.name),
    )?;
    Ok((top, ours))
}

fn run_git_merge_tree(top: &Path, base: &str, ours: &str, commit: &str) -> Result<(i32, MergeTree)> {
    let merged = git(
        top,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            &format!("--merge-base={base}"),
            ours,
            commit,
        ],
    )?;
    if merged.code != 0 && merged.code != 1 {
        return Err(merge_tree_error(merged.code, &merged.stderr));
    }
    Ok((merged.code, parse_merge_tree(&merged.stdout)))
}

fn finish_clean_merge(top: &Path, ours: &str, tree: &str, changes: Vec<TreeChange>) -> Result<MergeOutcome> {
    let moved = drifted(top, &changes)?;
    if !moved.is_empty() {
        return Ok(drift_conflict(moved));
    }
    write_changes(top, tree, &changes)?;
    let stat = git(top, &["diff", "--shortstat", ours, tree])?;
    Ok(MergeOutcome::Merged {
        files: changes.into_iter().map(|c| c.path).collect(),
        diffstat: stat.stdout.trim().to_string(),
    })
}

/// Merge a child's branch into the working tree at `target_cwd` as uncommitted changes. `markers` writes the
/// conflicted result with conflict markers instead of leaving the tree as it was.
pub fn merge_child_worktree(target_cwd: &Path, source: &MergeSource, markers: bool) -> Result<MergeOutcome> {
    let Some(commit) = source.commit.as_deref() else {
        return Ok(MergeOutcome::Empty);
    };
    let (top, ours) = prepare_merge_target(target_cwd, source, commit)?;
    let (code, parsed) = run_git_merge_tree(&top, &source.base, &ours, commit)?;
    let changes = tree_changes(&top, &ours, &parsed.tree)?;
    if changes.is_empty() && code == 0 {
        return Ok(MergeOutcome::Empty);
    }
    if code == 1 {
        return finish_conflicted_merge(&top, &ours, commit, &source.branch, parsed, &changes, markers);
    }
    finish_clean_merge(&top, &ours, &parsed.tree, changes)
}

fn unlink_tree(dir: &Path) {
    for entry in sorted_entries(dir) {
        let path = dir.join(entry);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            unlink_tree(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// The directory git commands for a record run in, and the `--git-dir` argument when the checkout is gone.
fn record_git(record: &WorktreeRecord) -> (PathBuf, Option<String>) {
    if Path::new(&record.repo).exists() {
        (PathBuf::from(&record.repo), None)
    } else {
        (
            PathBuf::from(&record.common_dir),
            Some(format!("--git-dir={}", record.common_dir)),
        )
    }
}

/// Remove a worktree's directory (its setup links first, never what they point to) and, optionally, its branch.
/// Best effort, like the TS removal.
pub fn remove_child_worktree(record: &WorktreeRecord, delete_branch: bool) {
    for path in record.linked.iter().chain(&record.copied) {
        let target = Path::new(&record.path).join(path);
        let Ok(info) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if info.file_type().is_symlink() || info.is_file() {
            let _ = std::fs::remove_file(&target);
        } else if info.is_dir() {
            unlink_tree(&target);
        }
    }
    let (cwd, git_dir) = record_git(record);
    let with = |args: &[&str]| -> Vec<String> {
        git_dir
            .iter()
            .cloned()
            .chain(args.iter().map(std::string::ToString::to_string))
            .collect()
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = git(&cwd, &refs);
    };
    run(with(&quiet(&[
        "worktree",
        "remove",
        "--force",
        "--force",
        &record.path,
    ])));
    let _ = std::fs::remove_dir_all(&record.path);
    run(with(&["worktree", "prune"]));
    if delete_branch {
        run(with(&["branch", "-D", "--", &record.branch]));
    }
    delete_record(record);
}

/// Whether a branch holds work beyond the base it started at.
fn branch_has_work(record: &WorktreeRecord) -> bool {
    let (cwd, git_dir) = record_git(record);
    let prefix: Vec<&str> = git_dir.iter().map(String::as_str).collect();
    let tip_ref = format!("refs/heads/{}", record.branch);
    let mut args = prefix.clone();
    args.extend(["rev-parse", "--verify", "-q", tip_ref.as_str()]);
    let Ok(tip) = git(&cwd, &args) else {
        return false;
    };
    if tip.code != 0 {
        return false;
    }
    let tip = tip.stdout.trim().to_string();
    let mut args = prefix;
    args.extend(["diff", "--quiet", record.base.as_str(), tip.as_str()]);
    git(&cwd, &args).is_ok_and(|same| same.code != 0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<String>,
    /// Branches kept because they hold work.
    pub kept_branches: Vec<String>,
}

#[derive(Default)]
pub struct PruneOptions<'a> {
    pub select: Option<&'a dyn Fn(&WorktreeRecord) -> bool>,
    /// Also take records whose owner lives.
    pub force: bool,
    pub delete_branches: bool,
}

/// [`prune_worktrees`] of every record whose owner is gone, as session `session_id` starts, but that session's own:
/// its open children are reattached with their worktrees (the durable host resumes them; TypeScript, which does not,
/// prunes them too). A repository without Ultron's worktrees directory has nothing of ours to prune: no `git
/// worktree prune` runs there.
pub fn prune_orphan_worktrees(cwd: &Path, session_id: &str) -> PruneReport {
    let Ok(info) = repo_info(cwd) else {
        return PruneReport::default();
    };
    if !records_dir(&info.common_dir).exists() {
        return PruneReport::default();
    }
    let others = |record: &WorktreeRecord| record.session_id != session_id;
    let mut report = prune_worktrees(
        cwd,
        &PruneOptions {
            select: Some(&others),
            ..PruneOptions::default()
        },
    );
    remove_torn(&info, &read_records(&info.common_dir), &mut report);
    report
}

/// Directories of ours with no record (a torn record) are leftovers too.
fn remove_torn(info: &RepoInfo, records: &[WorktreeRecord], report: &mut PruneReport) {
    let dir = records_dir(&info.common_dir);
    for entry in sorted_entries(&dir) {
        if entry.ends_with(".json") || records.iter().any(|r| r.id == entry) {
            continue;
        }
        let path = text(&dir.join(&entry));
        let _ = git(&info.top, &quiet(&["worktree", "remove", "--force", "--force", &path]));
        let _ = std::fs::remove_dir_all(&path);
        report.removed.push(path);
    }
}

fn prune_one_record(record: &WorktreeRecord, options: &PruneOptions<'_>, report: &mut PruneReport) {
    if options.select.is_some_and(|select| !select(record)) {
        return;
    }
    if !options.force && owner_alive(&record.owner) {
        return;
    }
    if Path::new(&record.path).exists() && (record.state == "active" || record.state == "creating") {
        let message = commit_message(
            &format!(
                "Uncommitted work of sub-agent {}, saved when its worktree was pruned",
                record.name
            ),
            &record.name,
            record.task_id.as_deref(),
            "interrupted",
        );
        let _ = commit_child_worktree(record, &message, None);
    }
    let keep = !options.delete_branches && branch_has_work(record);
    remove_child_worktree(record, !keep);
    report.removed.push(record.path.clone());
    if keep {
        report.kept_branches.push(record.branch.clone());
    }
}

/// Remove the worktrees of owners that are gone: uncommitted work is committed on the branch first, the directory is
/// removed, and the branch deleted unless it holds work. Then `git worktree prune`.
pub fn prune_worktrees(cwd: &Path, options: &PruneOptions<'_>) -> PruneReport {
    let Ok(info) = repo_info(cwd) else {
        return PruneReport::default();
    };
    let mut report = PruneReport::default();
    let records = read_records(&info.common_dir);
    for record in &records {
        prune_one_record(record, options, &mut report);
    }
    if options.select.is_none() {
        remove_torn(&info, &records, &mut report);
    }
    let _ = git(&info.top, &["worktree", "prune"]);
    report
}

impl CommitOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "commit": self.commit, "changedFiles": self.changed_files, "diffstat": self.diffstat })
    }
}

impl ConflictFile {
    pub fn to_json(&self) -> serde_json::Value {
        let hunks: Vec<serde_json::Value> = self
            .hunks
            .iter()
            .map(|h| serde_json::json!({ "line": h.line, "ours": h.ours, "theirs": h.theirs }))
            .collect();
        serde_json::json!({ "path": self.path, "kind": self.kind, "hunks": hunks })
    }
}

impl MergeOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            MergeOutcome::Merged { files, diffstat } => {
                serde_json::json!({ "status": "merged", "files": files, "diffstat": diffstat })
            }
            MergeOutcome::Empty => serde_json::json!({ "status": "empty" }),
            MergeOutcome::Conflict {
                files,
                conflicts,
                markers,
                message,
            } => serde_json::json!({
                "status": "conflict",
                "files": files,
                "conflicts": conflicts.iter().map(ConflictFile::to_json).collect::<Vec<_>>(),
                "markers": markers,
                "message": message,
            }),
        }
    }
}

impl PruneReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "removed": self.removed, "keptBranches": self.kept_branches })
    }
}
