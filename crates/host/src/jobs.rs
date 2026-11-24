//! Durable shell jobs for [`Host`]: spawning the keeper process, following its raw output into the cleaned spill file
//! and journal, waiting/cancelling, and restoring jobs across worker restarts.

use crate::journal::{JobRecord, Status};
use crate::shell;
use crate::text::{Decoder, clean, tail_bytes};
use crate::{
    CommandEnv, Host, JOB_MAX_ACTIVE, JOB_MAX_RETAINED, JOB_RETAINED_BYTES, JOB_TEXT_BYTES, JobEnd, JobEnds,
    PARENT_TOKEN_ENV, RootOf, Shared, command_env, safe_integer, sandboxed_argv,
};
use jsrt::sync;
use kernel::Signal;
use kernel::store::DocumentStore;
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StopReason {
    /// `job.cancel()`.
    Cancelled,
    /// Its root turn was aborted (Esc): stopped, and its end is not announced.
    RootAborted,
    /// The host shut down: nothing is reported.
    Closed,
}

pub(crate) struct Record {
    pub(crate) status: Status,
    pub(crate) exit_code: Option<i32>,
    pub(crate) started_at: u64,
    pub(crate) ended_at: Option<u64>,
    pub(crate) output_bytes: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) tail: String,
    pub(crate) error: Option<String>,
}

pub(crate) struct Job {
    pub(crate) id: String,
    /// The lane that started the job (it and the root lane see it) and that lane's root turn.
    pub(crate) lane: String,
    pub(crate) root_id: Option<String>,
    pub(crate) command: String,
    /// Where the command runs (`laneWorkspace(lane).cwd`, else the session's).
    pub(crate) cwd: String,
    pub(crate) timeout: Option<f64>,
    /// The cleaned, bounded output the kernel reads.
    pub(crate) output_path: PathBuf,
    /// The keeper's raw output and the exit status it writes when the command ends.
    pub(crate) raw_path: PathBuf,
    pub(crate) exit_path: PathBuf,
    pub(crate) record: Mutex<Record>,
    /// The keeper process (process group leader) and its start time.
    pub(crate) process: Mutex<Option<(i32, Option<u64>)>>,
    /// Set by the waiter of this worker's own keeper when it exits (its status code); guarded by `cancel.0`'s lock
    /// for wake-ups.
    pub(crate) exited: Mutex<Option<Option<i32>>>,
    /// Cancel requested; its condvar wakes the follower (cancel, keeper exit, release).
    pub(crate) cancel: (Mutex<bool>, Condvar),
    /// Why the first stop asked for it.
    pub(crate) reason: Mutex<Option<StopReason>>,
    /// Whether this worker still follows the job; its condvar wakes waiters and cancellers when that ends.
    pub(crate) tracking: (Mutex<Tracking>, Condvar),
    pub(crate) waiters: AtomicUsize,
}

/// Where this worker is with a job. Waiters and cancellers wait while it is `Following`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tracking {
    /// Its follower runs (or will run) and settles it.
    Following,
    /// Handed to the next owner of the journal ([`Host::release_jobs`]): this worker never settles it.
    Released,
    /// Settled: its record holds its end.
    Ended,
}

impl Job {
    pub(crate) fn new(
        id: String,
        (lane, root_id): (String, Option<String>),
        (command, cwd): (String, String),
        timeout: Option<f64>,
        jobs_dir: &Path,
        record: Record,
    ) -> Job {
        Job {
            output_path: jobs_dir.join(format!("{id}.log")),
            raw_path: jobs_dir.join(format!("{id}.out")),
            exit_path: jobs_dir.join(format!("{id}.exit")),
            id,
            lane,
            root_id,
            command,
            cwd,
            timeout,
            record: Mutex::new(record),
            process: Mutex::new(None),
            exited: Mutex::new(None),
            cancel: (Mutex::new(false), Condvar::new()),
            reason: Mutex::new(None),
            tracking: (Mutex::new(Tracking::Following), Condvar::new()),
            waiters: AtomicUsize::new(0),
        }
    }

    pub(crate) fn finished(&self) {
        self.stop_following(Tracking::Ended);
    }

    pub(crate) fn stop_following(&self, to: Tracking) {
        *sync::lock(&self.tracking.0) = to;
        self.tracking.1.notify_all();
    }

    pub(crate) fn following(&self) -> bool {
        *sync::lock(&self.tracking.0) == Tracking::Following
    }

    /// Waits until this worker settled the job or released it.
    pub(crate) fn wait_settled(&self) {
        let mut tracking = sync::lock(&self.tracking.0);
        while *tracking == Tracking::Following {
            tracking = self.tracking.1.wait(tracking).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The job's record; `tail`: with its output tail (the journal leaves it out).
pub(crate) fn job_record(job: &Job, tail: bool) -> JobRecord {
    let record = sync::lock(&job.record);
    let process = *sync::lock(&job.process);
    JobRecord {
        id: job.id.clone(),
        lane: job.lane.clone(),
        root_id: job.root_id.clone(),
        command: job.command.clone(),
        cwd: job.cwd.clone(),
        timeout: job.timeout,
        status: record.status,
        exit_code: record.exit_code.map(i64::from),
        started_at: record.started_at,
        ended_at: record.ended_at,
        output_path: job.output_path.to_string_lossy().into_owned(),
        output_bytes: record.output_bytes as u64,
        retained_bytes: record.retained_bytes as u64,
        tail: if tail { record.tail.clone() } else { String::new() },
        error: record.error.clone(),
        pid: process.map(|(pid, _)| i64::from(pid)),
        pid_start: process.and_then(|(_, start)| start),
    }
}

/// Start time of a live (not zombie) process, in clock ticks since boot (`/proc/<pid>/stat` field 22).
fn process_start(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// Whether the keeper recorded for a job still runs (same pid and start time).
fn keeper_alive(process: Option<(i32, Option<u64>)>) -> bool {
    match process {
        Some((pid, start)) => process_start(pid).is_some_and(|now| start.is_none_or(|start| start == now)),
        None => false,
    }
}

/// The exit status the keeper wrote, once the command has ended.
fn exit_status(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().last()?.parse().ok()
}

impl Host {
    /// Makes shell jobs durable: restores the jobs journal from `journal` and journals every start and end from now
    /// on. A job the journal lists as running is settled from its process: followed again while its keeper runs,
    /// completed with the exit status the keeper wrote, or `lost` when the process ended without one. Records
    /// without a process (TypeScript journals) read as `interrupted`, as there. Call once, before the first request.
    pub fn with_journal(self, journal: Arc<dyn DocumentStore>) -> Result<Host, String> {
        let now = jsrt::date::now_ms() as u64;
        let restored = crate::journal::restore_durable(journal.read()?.as_ref(), now)?;
        let mut adopted = Vec::new();
        {
            let mut jobs = sync::lock(&self.shared.jobs);
            for saved in restored.records {
                let job = Arc::new(Job::new(
                    saved.id.clone(),
                    (saved.lane.clone(), saved.root_id.clone()),
                    (saved.command.clone(), saved.cwd.clone()),
                    saved.timeout,
                    Path::new(&saved.output_path).parent().unwrap_or(&self.jobs_dir),
                    Record {
                        status: saved.status,
                        exit_code: saved.exit_code.and_then(|code| i32::try_from(code).ok()),
                        started_at: saved.started_at,
                        ended_at: saved.ended_at,
                        output_bytes: saved.output_bytes as usize,
                        retained_bytes: saved.retained_bytes as usize,
                        tail: saved.tail.clone(),
                        error: saved.error.clone(),
                    },
                ));
                if saved.status == Status::Running {
                    let pid = saved.pid.and_then(|pid| i32::try_from(pid).ok());
                    *sync::lock(&job.process) = pid.map(|pid| (pid, saved.pid_start));
                    adopted.push(job.clone());
                } else {
                    job.finished();
                }
                jobs.push(job);
            }
        }
        *sync::lock(&self.shared.journal) = Some(journal);
        for job in adopted {
            if keeper_alive(*sync::lock(&job.process)) {
                let (runner, shared) = (job, self.shared.clone());
                std::thread::spawn(move || follow(&runner, None, &shared));
            } else {
                follow(&job, None, &self.shared);
            }
        }
        self.shared.persist();
        Ok(self)
    }

    /// Hands the running jobs to the next owner of the journal: this host stops following and journaling them, and
    /// dropping it no longer stops them (a worker handing over to its successor). Without a journal it changes
    /// nothing but the drop.
    ///
    /// A stop already asked for is carried out first, so the successor does not keep following a job the person
    /// cancelled. The release is decided under the jobs lock, as each job's end and each stop request are: a job
    /// either ends here (and the journal says so) or is the successor's, and a later stop here returns at once.
    pub fn release_jobs(&self) {
        loop {
            let jobs = sync::lock(&self.shared.jobs);
            let stopping: Vec<Arc<Job>> = jobs
                .iter()
                .filter(|job| *sync::lock(&job.cancel.0) && job.following())
                .cloned()
                .collect();
            if stopping.is_empty() {
                self.shared.persist_locked(&jobs);
                self.shared.released.store(true, Ordering::SeqCst);
                break;
            }
            drop(jobs);
            for job in stopping {
                job.wait_settled();
            }
        }
        for job in sync::lock(&self.shared.jobs).iter() {
            job.cancel.1.notify_all();
        }
    }

    /// Stops every running job and waits until each has ended (worker shutdown). Dropping the host does the same
    /// unless [`Host::release_jobs`] was called. Nothing is reported of them.
    pub fn stop_jobs(&self) {
        let jobs = sync::lock(&self.shared.jobs).clone();
        for job in jobs {
            cancel(&job, StopReason::Closed, &self.shared);
        }
    }

    /// Who hears of every job's end (`onEnd`): the completion events of jobs nobody waited on.
    pub fn on_job_end(&self, ends: JobEnds) {
        *sync::lock(&self.shared.ends) = Some(ends);
    }

    /// Where a new job's usage root comes from (`host.rootOf(caller)`).
    pub fn set_roots(&self, roots: RootOf) {
        *sync::lock(&self.shared.roots) = Some(roots);
    }

    /// `cancelRoot`: Esc aborted a root turn: stop every job started under it (their ends are not announced).
    pub fn cancel_root(&self, root: &str) {
        let jobs: Vec<Arc<Job>> = sync::lock(&self.shared.jobs)
            .iter()
            .filter(|job| job.root_id.as_deref() == Some(root))
            .cloned()
            .collect();
        for job in jobs {
            cancel(&job, StopReason::RootAborted, &self.shared);
        }
    }

    /// How many callers are waiting on job `id` now (a waiter that sees the job end gets its result inline, so
    /// the end is not announced).
    pub fn job_waiters(&self, id: &str) -> usize {
        sync::lock(&self.shared.jobs)
            .iter()
            .find(|job| job.id == id)
            .map_or(0, |job| job.waiters.load(Ordering::SeqCst))
    }

    /// Every job's record, newest first (`ShellJobs.list()`), as `lane` may see them: the root lane every job, another
    /// lane its own.
    pub fn job_records(&self, lane: &str) -> Vec<JobRecord> {
        sync::lock(&self.shared.jobs)
            .iter()
            .rev()
            .filter(|job| visible(job, lane))
            .map(|job| job_record(job, true))
            .collect()
    }

    pub fn running_jobs(&self) -> usize {
        sync::lock(&self.shared.jobs)
            .iter()
            .filter(|job| sync::lock(&job.record).status == Status::Running)
            .count()
    }

    /// Running jobs of `lane` whose root `excluded` does not exclude (`running`).
    pub fn running(&self, lane: &str, excluded: &dyn Fn(Option<&str>) -> bool) -> usize {
        sync::lock(&self.shared.jobs)
            .iter()
            .filter(|job| job.lane == lane && !excluded(job.root_id.as_deref()))
            .filter(|job| sync::lock(&job.record).status == Status::Running)
            .count()
    }

    /// Plain `await bash(cmd)` with a yield_after: wait like the blocking bash, then leave a slow command running.
    pub(crate) fn shell_bash(
        &self,
        lane: &str,
        payload: &Map<String, Value>,
        signal: &Signal,
    ) -> Result<Value, String> {
        kernel::ops::fields("bash", payload)?;
        let command = crate::command_of(payload)?;
        let timeout = crate::timeout_of(payload.get("timeout"))?;
        let yield_after = crate::wait_seconds(payload.get("yield_after"), "yield_after")?;
        let job = self.start_job(lane, command, timeout)?;
        wait(&job, yield_after, signal);
        if sync::lock(&job.record).status == Status::Running && signal.is_aborted() {
            job.waiters.fetch_add(1, Ordering::SeqCst);
            cancel(&job, StopReason::Cancelled, &self.shared);
            job.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        bash_result(&job)
    }

    pub(crate) fn find_job(&self, lane: &str, id: Option<&Value>) -> Result<Arc<Job>, String> {
        let Some(Value::String(id)) = id.filter(|id| id.as_str().is_some_and(|id| !id.is_empty())) else {
            return Err("job id must be a nonempty string".into());
        };
        sync::lock(&self.shared.jobs)
            .iter()
            .find(|job| &job.id == id && visible(job, lane))
            .cloned()
            .ok_or_else(|| format!("Unknown shell job {id}"))
    }

    pub(crate) fn start_job(&self, lane: &str, command: String, timeout: Option<f64>) -> Result<Arc<Job>, String> {
        let root = sync::lock(&self.shared.roots).clone().and_then(|roots| roots(lane));
        let mut jobs = sync::lock(&self.shared.jobs);
        let running = |job: &Arc<Job>| sync::lock(&job.record).status == Status::Running;
        if jobs.iter().filter(|job| running(job)).count() >= JOB_MAX_ACTIVE {
            return Err(format!("At most {JOB_MAX_ACTIVE} shell jobs can run at once"));
        }
        let id = format!("job-{}", jsrt::random::random_hex(4));
        let job = Arc::new(Job::new(
            id,
            (lane.to_string(), root),
            (command, self.cwd.to_string_lossy().into_owned()),
            timeout,
            &self.jobs_dir,
            Record {
                status: Status::Running,
                exit_code: None,
                started_at: jsrt::date::now_ms() as u64,
                ended_at: None,
                output_bytes: 0,
                retained_bytes: 0,
                tail: String::new(),
                error: None,
            },
        ));
        jobs.push(job.clone());
        let excess = jobs.len().saturating_sub(JOB_MAX_RETAINED);
        let pruned: Vec<String> = jobs
            .iter()
            .filter(|job| !running(job))
            .take(excess)
            .map(|job| job.id.clone())
            .collect();
        jobs.retain(|job| {
            let keep = !pruned.contains(&job.id);
            if !keep {
                for path in [&job.output_path, &job.raw_path, &job.exit_path] {
                    let _ = std::fs::remove_file(path);
                }
            }
            keep
        });
        drop(jobs);
        let (started, spawned) = mpsc::channel();
        let (runner, shared, cwd, launcher, shell) = (
            job.clone(),
            self.shared.clone(),
            (self.cwd.clone(), self.base_env.clone(), self.env.clone()),
            self.launcher.clone(),
            self.shell()
                .and_then(|shell| Ok((shell, self.shell_named(None)?.shell))),
        );
        std::thread::spawn(move || {
            let env = CommandEnv {
                cwd: &cwd.0,
                base: cwd.1.as_deref().map(Vec::as_slice),
                env: &cwd.2,
            };
            match shell.and_then(|(shell, keeper)| spawn_keeper(&runner, (&shell, &keeper), env, launcher.as_deref())) {
                Ok(child) => {
                    let pid = child.id() as i32;
                    *sync::lock(&runner.process) = Some((pid, process_start(pid)));
                    let _ = started.send(());
                    follow(&runner, Some(child), &shared);
                }
                Err(error) => {
                    let mut record = sync::lock(&runner.record);
                    record.status = Status::Failed;
                    record.ended_at = Some(jsrt::date::now_ms() as u64);
                    record.error = Some(error);
                    drop(record);
                    let _ = started.send(());
                    runner.finished();
                }
            }
        });
        let _ = spawned.recv();
        self.shared.persist();
        Ok(job)
    }
}

/// The keeper: runs the command with the session's shell (`$1 $2 "$0"`: the shell and its `-c`; stdout and stderr
/// together into the raw file, which is the keeper's stdout), then writes the command's exit status to its own
/// stderr, the exit file. Files, not pipes, so the command keeps running and its end is recorded when the worker
/// that started it is gone. The keeper itself is POSIX shell run by bash, whatever shell the command uses.
const KEEPER: &str = r#""$1" "$2" "$0" 2>&1 </dev/null; printf '%s\n' "$?" >&2"#;

fn spawn_keeper(
    job: &Job,
    (shell, keeper): (&shell::ShellConfig, &str),
    CommandEnv { cwd, base, env }: CommandEnv<'_>,
    launcher: Option<&sandbox::Launcher>,
) -> Result<std::process::Child, String> {
    let create = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    let raw = create(&job.raw_path)?;
    let exit = create(&job.exit_path)?;
    let mut argv = vec![keeper.to_string(), "-c".into(), KEEPER.into(), job.command.clone()];
    argv.push(shell.shell.clone());
    argv.extend(shell.args.iter().cloned());
    let mut process = match launcher {
        Some(launcher) => launcher.command_persistent(&sandboxed_argv(argv, env), cwd)?,
        None => {
            let mut process = Command::new(&argv[0]);
            process.args(&argv[1..]).current_dir(cwd);
            process
        }
    };
    let isolated = launcher.is_some_and(|launcher| launcher.mechanism().is_some());
    command_env(&mut process, base, env, isolated);
    process
        .env_remove(PARENT_TOKEN_ENV)
        .stdin(Stdio::null())
        .stdout(raw)
        .stderr(exit)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())
}

/// Turns raw output into the cleaned, bounded spill file and the record's counts and tail, as the TypeScript job
/// does with its stream.
struct Digest {
    decoder: Decoder,
    log: Option<File>,
}

impl Digest {
    /// Starts the spill file over: a job followed again after a restart is digested from its first raw byte.
    fn new(job: &Job) -> Digest {
        let log = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&job.output_path)
            .ok();
        let mut record = sync::lock(&job.record);
        record.output_bytes = 0;
        record.retained_bytes = 0;
        record.tail.clear();
        Digest {
            decoder: Decoder::default(),
            log,
        }
    }

    fn text(&mut self, job: &Job, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut record = sync::lock(&job.record);
        record.output_bytes += text.len();
        let room = JOB_RETAINED_BYTES.saturating_sub(record.retained_bytes);
        if room > 0 {
            let kept = &text.as_bytes()[..room.min(text.len())];
            if let Some(log) = self.log.as_mut() {
                let _ = log.write_all(kept);
            }
            record.retained_bytes += kept.len();
        }
        let tail = format!("{}{text}", record.tail);
        record.tail = tail_bytes(&tail, JOB_TEXT_BYTES / 2).to_owned();
    }

    /// Reads every raw byte written so far.
    fn pump(&mut self, job: &Job, raw: &mut Option<File>) {
        let Some(file) = raw.as_mut() else {
            return;
        };
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = file.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let text = clean(&self.decoder.decode(&buffer[..read], true));
            self.text(job, &text);
        }
    }

    fn finish(&mut self, job: &Job) {
        let text = clean(&self.decoder.decode(&[], false));
        self.text(job, &text);
    }
}

/// Interval for tailing a durable job's `.out` and `.exit` files while it runs.
const FOLLOW_POLL: Duration = Duration::from_millis(10);

/// Follows a job until it ends: its own child (`child`), or a keeper started by an earlier worker (`None`), whose
/// end is its exit file or, without one, its process going away (`lost`). Stops the process group on cancel or
/// timeout. Returns without settling the job when the host released its jobs.
fn follow(job: &Arc<Job>, child: Option<std::process::Child>, shared: &Shared) {
    let own = child.is_some();
    if let Some(mut child) = child {
        let waiter = job.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|status| status.code());
            let guard = sync::lock(&waiter.cancel.0);
            *sync::lock(&waiter.exited) = Some(code);
            drop(guard);
            waiter.cancel.1.notify_all();
        });
    }
    let mut digest = Digest::new(job);
    let mut raw = File::open(&job.raw_path).ok();
    let started_at = sync::lock(&job.record).started_at;
    let deadline = job
        .timeout
        .map(|seconds| started_at.saturating_add((seconds * 1000.0) as u64));
    let process = *sync::lock(&job.process);
    let mut stopped: Option<Status> = None;
    let ended = loop {
        let ended = match own {
            true => *sync::lock(&job.exited),
            false => {
                let code = exit_status(&job.exit_path);
                (code.is_some() || !keeper_alive(process)).then_some(code)
            }
        };
        digest.pump(job, &mut raw);
        if let Some(ended) = ended {
            break ended;
        }
        if shared.released.load(Ordering::SeqCst) {
            job.stop_following(Tracking::Released);
            return;
        }
        if stopped.is_none() {
            if deadline.is_some_and(|deadline| jsrt::date::now_ms() as u64 >= deadline) {
                stopped = Some(Status::TimedOut);
            } else if *sync::lock(&job.cancel.0) {
                stopped = Some(Status::Cancelled);
            }
            if stopped.is_some()
                && let Some((pid, _)) = process
            {
                kernel::kill_command_group(pid);
            }
        }
        let cancel = sync::lock(&job.cancel.0);
        if sync::lock(&job.exited).is_none() {
            drop(
                job.cancel
                    .1
                    .wait_timeout(cancel, FOLLOW_POLL)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    };
    digest.finish(job);
    let code = exit_status(&job.exit_path);
    let jobs = sync::lock(&shared.jobs);
    if shared.released.load(Ordering::SeqCst) {
        drop(jobs);
        job.stop_following(Tracking::Released);
        return;
    }
    {
        let mut record = sync::lock(&job.record);
        record.ended_at = Some(match own {
            true => jsrt::date::now_ms() as u64,
            false => std::fs::metadata(&job.exit_path)
                .and_then(|meta| meta.modified())
                .ok()
                .filter(|_| code.is_some())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or_else(|| jsrt::date::now_ms() as u64, |d| d.as_millis() as u64),
        });
        match stopped {
            Some(status) => {
                record.status = status;
                record.exit_code = None;
                if status == Status::TimedOut {
                    record.error = Some(format!(
                        "timed out after {}s; process tree killed",
                        jsrt::number::to_string(job.timeout.unwrap_or(0.0))
                    ));
                }
            }
            None => match (code, own) {
                (Some(code), _) => {
                    record.status = Status::Completed;
                    record.exit_code = Some(code);
                }
                (None, true) => {
                    record.status = Status::Completed;
                    record.exit_code = ended;
                }
                (None, false) => {
                    record.status = Status::Lost;
                    record.exit_code = None;
                    record.error =
                        Some("its process ended while no Ultron worker was running; the exit status is unknown".into());
                }
            },
        }
    }
    shared.persist_locked(&jobs);
    drop(jobs);
    let reason = *sync::lock(&job.reason);
    let end = JobEnd {
        record: job_record(job, true),
        awaited: job.waiters.load(Ordering::SeqCst) > 0,
        root_aborted: reason == Some(StopReason::RootAborted),
    };
    let ends = sync::lock(&shared.ends).clone();
    job.finished();
    if reason != Some(StopReason::Closed)
        && let Some(ends) = ends
    {
        ends(end);
    }
}

/// Wait for a job at most `seconds` (at least 20 ms; None: until it ends) or until the calling cell ends.
pub(crate) fn wait(job: &Arc<Job>, seconds: Option<f64>, signal: &Signal) {
    job.waiters.fetch_add(1, Ordering::SeqCst);
    let deadline = seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds.max(0.02)));
    let woken = job.clone();
    let link = signal.link(move |_| {
        let _tracking = sync::lock(&woken.tracking.0);
        woken.tracking.1.notify_all();
    });
    let mut done = sync::lock(&job.tracking.0);
    while *done == Tracking::Following && !signal.is_aborted() {
        done = match deadline {
            None => job.tracking.1.wait(done).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                job.tracking
                    .1
                    .wait_timeout(done, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
    }
    drop(done);
    drop(link);
    job.waiters.fetch_sub(1, Ordering::SeqCst);
}

/// Stops a job this worker follows and waits until it is settled (or released). A job already handed to the next
/// owner is not this worker's to stop: it returns at once.
pub(crate) fn cancel(job: &Job, reason: StopReason, shared: &Shared) {
    {
        let _jobs = sync::lock(&shared.jobs);
        if shared.released.load(Ordering::SeqCst) || !job.following() {
            return;
        }
        sync::lock(&job.reason).get_or_insert(reason);
        *sync::lock(&job.cancel.0) = true;
    }
    job.cancel.1.notify_all();
    job.wait_settled();
}

fn read_file(path: &Path, cursor: usize, max: usize) -> String {
    let mut buffer = Vec::new();
    if let Ok(mut file) = File::open(path)
        && file.seek(SeekFrom::Start(cursor as u64)).is_ok()
    {
        let _ = file.take(max as u64).read_to_end(&mut buffer);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Whole output when it fits, else the first and last halves around a marker naming how to read the rest.
fn job_text(job: &Job, record: &Record) -> (String, bool) {
    let half = JOB_TEXT_BYTES / 2;
    if record.output_bytes <= JOB_TEXT_BYTES && record.retained_bytes == record.output_bytes {
        return (read_file(&job.output_path, 0, JOB_TEXT_BYTES), false);
    }
    let head = read_file(&job.output_path, 0, half);
    let omitted = record.output_bytes.saturating_sub(half + record.tail.len());
    let marker = format!(
        "\n[... {omitted} bytes omitted; {} bytes kept in {}: await job.read(cursor={half}, max_bytes=65536) ...]\n",
        record.retained_bytes,
        job.output_path.display()
    );
    (format!("{head}{marker}{}", record.tail), true)
}

pub(crate) fn snapshot(job: &Job) -> Value {
    let record = sync::lock(&job.record);
    let (text, truncated) = job_text(job, &record);
    let elapsed = record
        .ended_at
        .unwrap_or_else(|| jsrt::date::now_ms() as u64)
        .saturating_sub(record.started_at);
    json!({
        "id": job.id,
        "lane": job.lane,
        "command": job.command,
        "status": record.status.as_str(),
        "running": record.status == Status::Running,
        "exit_code": record.exit_code,
        "ok": record.status == Status::Completed && record.exit_code == Some(0),
        "timed_out": record.status == Status::TimedOut,
        "cancelled": record.status == Status::Cancelled,
        "text": text,
        "truncated": truncated,
        "output_path": job.output_path.to_string_lossy(),
        "output_bytes": record.output_bytes,
        "elapsed_seconds": jsrt::json::number((elapsed as f64 / 100.0).round() / 10.0),
        "error": record.error,
    })
}

/// The blocking bash's reply shape for a command that ran as a job, plus `running` and the job while it runs.
fn bash_result(job: &Job) -> Result<Value, String> {
    let (status, exit_code, error, partial) = {
        let record = sync::lock(&job.record);
        (
            record.status,
            record.exit_code,
            record.error.clone(),
            record.retained_bytes < record.output_bytes,
        )
    };
    if status == Status::Failed {
        return Err(error.unwrap_or_else(|| "bash command failed".into()));
    }
    let snapshot = snapshot(job);
    let truncated = snapshot["truncated"].as_bool() == Some(true);
    let running = status == Status::Running;
    Ok(json!({
        "output": snapshot["text"],
        "exit_code": exit_code,
        "cancelled": status == Status::Cancelled,
        "timed_out": status == Status::TimedOut,
        "truncated": truncated,
        "full_output_path": if truncated { snapshot["output_path"].clone() } else { Value::Null },
        "partial_file": partial,
        "running": running,
        "job": if running { snapshot } else { Value::Null },
    }))
}

pub(crate) fn read_output(job: &Job, cursor: Option<&Value>, max_bytes: Option<&Value>) -> Result<Value, String> {
    let cursor = safe_integer(cursor, 0).ok_or("cursor must be a non-negative integer")? as usize;
    let max = safe_integer(max_bytes, 65_536)
        .filter(|max| (1..=1_048_576).contains(max))
        .ok_or("max_bytes must be an integer between 1 and 1048576")? as usize;
    let record = sync::lock(&job.record);
    let start = cursor.min(record.retained_bytes);
    let text = read_file(&job.output_path, start, max.min(record.retained_bytes - start));
    let next = start + text.len();
    Ok(json!({
        "text": text,
        "next_cursor": next,
        "done": record.status != Status::Running && next >= record.retained_bytes,
        "truncated": record.retained_bytes < record.output_bytes,
    }))
}

pub(crate) fn visible(job: &Job, lane: &str) -> bool {
    kernel::sees(lane, &job.lane)
}
