//! Dream mode (Rust-only): the durable instance learns while nobody uses it. Each server generation of the durable
//! instance keeps one daily schedule ([`SCHEDULE_NAME`]) in its own session (the dream session, its id in the
//! instance's [`SESSION_FILE`]); the schedule fires at `learn.dreamAt` local time (default [`DEFAULT_AT`]) with
//! [`PROMPT`], which runs the bundled `ultron/dream` skill: it reads the recent sessions, autoreview's lessons, the
//! reviews of the user's own pull requests and Hindsight's banks, and keeps only what evidence backs, through the
//! gated APIs (`memory.propose`,
//! `memory.invalidate`, `skills.propose_code`), each change reversible. [`ensure`] reconciles the schedule to the
//! settings on every start: run twice it changes nothing; `learn.dream: false` (or `ULTRON_DREAM=off`, for tests)
//! cancels it. A night with nothing new (no session changed since the last dream) passes without a model run
//! ([`QuietDreams`]).

use crate::catalog;
use crate::prompt_schedules::{Deliver, Delivery};

use jsrt::localtime::Zone;
use kernel_apis::TickClock;
use kernel_apis::prompt_schedule::{Kind, Schedule, State};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The dream session's id, in the instance directory.
pub const SESSION_FILE: &str = "dream-session";
pub const SCHEDULE_NAME: &str = "dream";
pub const SESSION_NAME: &str = "Dream";
pub const DEFAULT_AT: &str = "03:30";
const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;
pub const PROMPT: &str = "Dream: consolidate what you learned since the last dream. Read `ultron/dream/SKILL.md` \
    from the bundled skills directory and follow it. This run is unattended: nobody is here to answer.";

/// `learn.dreamAt` as `(hour, minute)`: `"HH:MM"` (24-hour), else [`DEFAULT_AT`].
pub fn dream_at(settings: &Value) -> (u32, u32) {
    let parse = |text: &str| {
        let (h, m) = text.trim().split_once(':')?;
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some((h, m))
    };
    settings["learn"]["dreamAt"]
        .as_str()
        .and_then(parse)
        .or_else(|| parse(DEFAULT_AT))
        .unwrap_or((3, 30))
}

/// The first `hour:minute` local time after `now`.
pub fn next_slot(now: i64, (hour, minute): (u32, u32), zone: &Zone) -> i64 {
    let offset = zone.offset(now as f64) * 1000;
    let day = (now + offset).div_euclid(DAY_MS) * DAY_MS - offset;
    let slot = day + i64::from(hour * 60 + minute) * MINUTE_MS;
    if slot > now { slot } else { slot + DAY_MS }
}

/// The dream session's id when its file names a session of `session_dir`.
pub fn session_id(instance_dir: &Path, session_dir: &Path) -> Option<String> {
    let id = std::fs::read_to_string(instance_dir.join(SESSION_FILE)).ok()?;
    let id = id.trim();
    catalog::list(session_dir)
        .into_iter()
        .any(|m| m.id == id)
        .then(|| id.to_string())
}

/// Bring the dream schedule in line with `settings` (`learn.dream`, default on; `learn.dreamAt`): the dream session
/// is created in `cwd` when missing, its active `dream` schedule is kept when it is daily at the set time and runs
/// [`PROMPT`], else replaced; off cancels it. The active schedule after it, if any.
pub fn ensure(
    instance_dir: &Path,
    session_dir: &Path,
    cwd: &Path,
    settings: &Value,
    clock: Arc<dyn TickClock>,
    zone: &Zone,
) -> Result<Option<Schedule>, String> {
    let on = settings["learn"]["dream"].as_bool().unwrap_or(true);
    let id = match (session_id(instance_dir, session_dir), on) {
        (Some(id), _) => id,
        (None, false) => return Ok(None),
        (None, true) => create_session(instance_dir, session_dir, cwd)?,
    };
    let meta = catalog::list(session_dir)
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("Unknown session: {id}"))?;
    let at = dream_at(settings);
    crate::prompt_schedules::in_journal(&meta.path, clock, |module| {
        let current = module
            .all()?
            .into_iter()
            .find(|s| s.active() && s.name == SCHEDULE_NAME);
        // ponytail: a fixed 1-day interval drifts an hour across a DST change until the next server start re-aims it.
        let fits = |s: &Schedule| {
            let local = s.next_due.map(|due| zone.fields(due as f64));
            s.prompt == PROMPT && s.interval_ms == Some(DAY_MS) && local.is_some_and(|f| (f.hour, f.minute) == at)
        };
        match current {
            Some(s) if on && fits(&s) => return Ok(Some(s)),
            Some(s) => {
                module.cancel(&s.id)?;
            }
            None => {}
        }
        if !on {
            return Ok(None);
        }
        let now = module.now();
        let schedule = Schedule {
            id: format!("sch-{}", &jsrt::random::uuid_v4().replace('-', "")[..8]),
            name: SCHEDULE_NAME.into(),
            session_id: id.clone(),
            prompt: PROMPT.into(),
            kind: Kind::Every,
            interval_ms: Some(DAY_MS),
            until: None,
            max_runs: None,
            created_at: now,
            next_due: Some(next_slot(now, at, zone)),
            runs: 0,
            last_fired: None,
            state: State::Active,
            operation: None,
        };
        module.put(&schedule)?;
        Ok(Some(schedule))
    })
}

/// Whether a session of `roots` (the stores `sessions.list(cwd="all")` reads) other than the dream session `dream`
/// changed after `since` (the dream's last run, ms); never dreamed is yes.
pub fn changed_since(roots: &[PathBuf], dream: &str, since: Option<i64>) -> bool {
    let Some(since) = since else {
        return true;
    };
    session_journal::search::list(roots, None, None)
        .iter()
        .any(|m| m.id != dream && m.modified_at > since as f64)
}

/// The scheduler's delivery with dreaming's quiet nights passed over: when the dream session's `dream` schedule is
/// due and no other session changed since its last run, the slot is skipped in the journal (no prompt, so no
/// model call) and the next night is due. Anything else, or a skip that cannot take the session's locks (its
/// worker is running), is delivered as usual.
pub struct QuietDreams {
    pub inner: Arc<dyn Deliver>,
    pub instance_dir: PathBuf,
    pub session_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub home: PathBuf,
    pub clock: Arc<dyn TickClock>,
}

impl Deliver for QuietDreams {
    fn fire(&self, meta: &catalog::Meta, id: &str, due: i64) -> Delivery {
        let dream = session_id(&self.instance_dir, &self.session_dir).filter(|dream| *dream == meta.id);
        let quiet = dream.is_some()
            && kernel_apis::prompt_schedule::scan_journal(&meta.path)
                .iter()
                .find(|s| s.id == id && s.name == SCHEDULE_NAME)
                .is_some_and(|s| {
                    let roots =
                        session_core::compose::store_roots(&self.agent_dir, &self.home, Some(&self.session_dir));
                    !changed_since(&roots, &meta.id, s.last_fired)
                });
        if quiet {
            let skipped = crate::prompt_schedules::in_journal(&meta.path, self.clock.clone(), |module| {
                module.skip(id, due)?;
                module.entries()
            });
            if let Ok(active) = skipped {
                return Delivery::Done(Value::Array(active));
            }
        }
        self.inner.fire(meta, id, due)
    }
}

fn create_session(instance_dir: &Path, session_dir: &Path, cwd: &Path) -> Result<String, String> {
    let repo = session_journal::SessionRepo::new(session_dir, session_journal::SystemClock::shared());
    let mut session = repo.create(&cwd.display().to_string(), None, None)?;
    session.commit(&[session_journal::Write::set(
        session_journal::ns::SESSION_NAME,
        "",
        SESSION_NAME.into(),
    )])?;
    let id = session.metadata().id.clone();
    std::fs::create_dir_all(instance_dir).map_err(|e| e.to_string())?;
    let temp = instance_dir.join(format!("{SESSION_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&temp, format!("{id}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, instance_dir.join(SESSION_FILE)).map_err(|e| e.to_string())?;
    Ok(id)
}
