//! The engine's deterministic stage (`autoreview_static.py`: compile, lint, type-check, Loki) and the blind check of
//! confirmed findings (`autoreview_blind.py`: the judge seat over the claim and the code only), through the engine
//! Python over a fake `rlm`; `review.loki` as the engine serves it. No tool is required: ruff and mypy are scripted.

use kernel::{HostHandler, Signal};
use review::loki_scan::LokiScan;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_AUTHOR_NAME", "Review Test")
        .env("GIT_AUTHOR_EMAIL", "review@test")
        .env("GIT_COMMITTER_NAME", "Review Test")
        .env("GIT_COMMITTER_EMAIL", "review@test")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
}

/// A repository whose head commit breaks calc.py's syntax on line 2, adds `count` (undefined) on line 5 and a new
/// lib.py; config.json was already broken at the base. (dir, base, head)
fn repository() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("calc.py"), "def total(items):\n    return sum(items)\n").expect("calc");
    std::fs::write(repo.join("config.json"), "{\"a\": 1,}\n").expect("config");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("calc.py"),
        "def total(items):\n    return sum(items\n\ndef mean(items):\n    return total(items) / count\n",
    )
    .expect("calc");
    std::fs::write(repo.join("config.json"), "{\"a\": 2,}\n").expect("config");
    std::fs::write(repo.join("lib.py"), "def ok(x):\n    return undefined_thing + x\n").expect("lib");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

/// Runs `body` (Python; `a` is the engine module, `st` the stage's, with `repo`, `base`, `head` set and every
/// external tool scripted by `FAKE_TOOLS`) and parses what it prints.
fn engine(repo: &Path, base: &str, head: &str, body: &str) -> Value {
    let rlm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm");
    let script = format!(
        "import sys, json, asyncio\nsys.path.insert(0, {rlm:?})\nimport autoreview_api as a\nimport autoreview_static as st\n\
         repo, base, head = {repo:?}, {base:?}, {head:?}\n{FAKE_TOOLS}\n{body}",
        rlm = rlm.display().to_string(),
        repo = repo.display().to_string(),
    );
    let python = kernel::env::python(|name| std::env::var(name).ok());
    let output = Command::new(python)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("python runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("json")
}

/// ruff finds `undefined_thing` in lib.py (line 2) and, on a line the change did not add, one in calc.py; mypy is
/// not installed; node neither. What ran is recorded in `RAN`.
const FAKE_TOOLS: &str = "RAN = []\n\
def fake_which(name):\n    return {'ruff': '/fake/ruff'}.get(name)\n\
def fake_run(argv, cwd, env, timeout):\n\
\x20   RAN.append(argv)\n\
\x20   assert 'calc.py' not in argv, 'a file that does not compile is not linted'\n\
\x20   import os\n\
\x20   return 1, json.dumps([{'code': 'F821', 'message': 'Undefined name `undefined_thing`', 'filename': os.path.join(cwd, 'lib.py'),\n\
\x20                          'location': {'row': 2, 'column': 12}},\n\
\x20                         {'code': 'F821', 'message': 'Undefined name `x`', 'filename': os.path.join(cwd, 'lib.py'),\n\
\x20                          'location': {'row': 9, 'column': 1}}])\n\
st.shutil.which = fake_which\n\
st.testing.run_process = fake_run\n";

#[test]
fn review_static_stage_confirms_what_the_tools_find_on_added_lines_and_degrades_per_stage() {
    let (dir, base, head) = repository();
    let body = "scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)\n\
                out = asyncio.run(st.run_static(object(), scope.files, scope.read_file, a._rev_reader(git, base)))\n\
                print(json.dumps({'stages': {s['name']: [s['status'], s['note'], s['files']] for s in out['stages']},\n\
                \x20                 'findings': [[f['file'], f['line'], f['source'], f['host_confirmed'], f['claim']] for f in out['findings']],\n\
                \x20                 'clean': out['clean'], 'ran': [argv[0] for argv in RAN]}))\n";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(
        got["stages"],
        json!({
            "compile": ["findings", null, ["calc.py", "config.json", "lib.py"]],
            "lint": ["findings", null, ["lib.py"]],
            "typecheck": ["skipped", "mypy is not installed", []],
            "loki": ["skipped", "the host serves no Loki scan", []],
        })
    );
    // config.json was broken at the base too: not this change's. ruff's line 9 is not an added line.
    assert_eq!(
        got["findings"],
        json!([
            [
                "calc.py",
                2,
                "static:compile",
                true,
                "calc.py no longer compiles: SyntaxError: '(' was never closed (line 2)."
            ],
            [
                "lib.py",
                2,
                "static:lint",
                true,
                "Undefined name `undefined_thing` (F821)."
            ],
        ])
    );
    // Only lib.py compiled, and ruff flagged it: clean for compile alone. A file a tool found anything in (config.json
    // at the base too, calc.py) is clean for nothing, so it contradicts no claim.
    assert_eq!(got["clean"], json!({"lib.py": ["compile"]}));
    assert_eq!(got["ran"], json!(["/fake/ruff"]));
}

/// A fake `rlm`: the correctness finder raises a syntax error in lib.py (it compiles) and an undefined name in
/// calc.py's mean() (true: `count` is undefined, and calc.py does not compile, so nothing contradicts it); the
/// verifier confirms; `review.loki` answers one credential; `judge.decide` refutes after escalating.
const RLM: &str = "class Rlm:\n\
\x20   def __init__(self, judge):\n        self.requests = []\n        self.judge = judge\n\
\x20   async def host_request(self, kind, payload):\n\
\x20       self.requests.append([kind, payload])\n\
\x20       if kind == 'review.loki':\n\
\x20           assert [c[0] for c in payload['changes']] == ['calc.py', 'config.json', 'lib.py']\n\
\x20           return {'violations': ['lib.py:1: loki/secret: hardcoded credential (AWS access key)']}\n\
\x20       if kind == 'judge.decide':\n\
\x20           return self.judge\n\
\x20       raise RuntimeError(kind + ' is not available to the review engine')\n\
\x20   async def map(self, tasks, items, **options):\n\
\x20       if 'You check one finding' in tasks[0]:\n\
\x20           out = []\n\
\x20           for views in items:\n\
\x20               joined = '\\n'.join(views) if isinstance(views, list) else str(views)\n\
\x20               if 'loki/secret' in joined:\n\
\x20                   out.append({'verdict': 'confirmed', 'evidence': '`def ok(x):`', 'corrected_line': None,\n\
\x20                               'severity': 'high', 'scenario_holds': True})\n\
\x20               else:\n\
\x20                   out.append({'verdict': 'confirmed', 'evidence': '`return total(items) / count`', 'corrected_line': None,\n\
\x20                               'severity': 'high', 'scenario_holds': True})\n\
\x20           return out\n\
\x20       if 'Your specialty: Correctness' in tasks[0]:\n\
\x20           return [[{'file': 'lib.py', 'line': 1, 'end_line': 1, 'severity': 'high', 'category': 'correctness',\n\
\x20                     'claim': 'ok() has a syntax error: the definition does not parse.', 'why': 'w',\n\
\x20                     'suggested_fix': 'f', 'confidence': 0.9, 'replacement': None, 'scenario': 'import fails'},\n\
\x20                    {'file': 'calc.py', 'line': 5, 'end_line': 5, 'severity': 'high', 'category': 'performance',\n\
\x20                     'claim': 'mean() divides by count, which is not defined in the module.', 'why': 'SECRET-REASONING',\n\
\x20                     'suggested_fix': 'f', 'confidence': 0.9, 'replacement': None, 'scenario': 'mean([1]) raises'}]]\n\
\x20       return [[]]\n";

const RUN: &str = "rlm = Rlm(JUDGE)\n\
result = asyncio.run(a.run(rlm, {'repoDir': repo, 'base': base, 'head': head, 'mode': 'fast'}))\n\
asked = [p for k, p in rlm.requests if k == 'judge.decide']\n\
print(json.dumps({'findings': [[f['file'], f['line'], f['source'], f['verification'], f.get('blind'), f.get('note')]\n\
\x20                               for f in result['findings']],\n\
\x20                 'contradicted': result['static']['contradicted'], 'dropped': result['dropped']['contradicted'],\n\
\x20                 'blind': result['blind'], 'usage': [result['usage']['costUsd'], result['usage'].get('judge')],\n\
\x20                 'asked': asked}))\n";

#[test]
fn review_static_stage_drops_contradicted_findings_and_the_blind_check_refutes_after_escalation() {
    let (dir, base, head) = repository();
    let judge = "JUDGE = {'rung': 'escalated', 'settled': True, 'reason': 'disputed: shown is false, the caller holds true',\n\
                 \x20        'answers': {'shown': {'value': False, 'confidence': 0.9}},\n\
                 \x20        'receipts': [{'status': 'succeeded', 'cost_usd': None}, {'status': 'succeeded', 'cost_usd': 0.002}]}\n";
    let got = engine(dir.path(), &base, &head, &format!("{RLM}{judge}{RUN}"));
    // The syntax-error claim is contradicted by the compiler (lib.py compiles): dropped before the verifier.
    assert_eq!(
        got["contradicted"],
        json!([{"file": "lib.py", "line": 1, "claim": "ok() has a syntax error: the definition does not parse.",
            "source": "fast", "by": "compile ran clean on lib.py"}])
    );
    assert_eq!(got["dropped"], 1);
    let findings = got["findings"].as_array().unwrap();
    let place = |file: &str, line: i64, source: &str| {
        findings
            .iter()
            .find(|f| f[0] == file && f[1] == line && f[2] == source)
            .unwrap_or_else(|| panic!("{file}:{line} {source} in {findings:?}"))
    };
    assert_eq!(place("calc.py", 2, "static:compile")[3], "confirmed");
    assert_eq!(place("lib.py", 2, "static:lint")[3], "confirmed");
    // Loki findings now require a second-pass verifier (not host_confirmed), so the blind judge checks them too.
    let loki_refuted = place("lib.py", 1, "static:loki");
    assert_eq!(
        (loki_refuted[3].clone(), loki_refuted[4].clone()),
        (json!("uncertain"), json!("refuted"))
    );
    // The verifier confirmed the model finding; the blind check (escalated) refuted it: uncertain.
    let refuted = place("calc.py", 5, "fast");
    assert_eq!(
        (refuted[3].clone(), refuted[4].clone()),
        (json!("uncertain"), json!("refuted"))
    );
    assert!(refuted[5].as_str().unwrap().starts_with("the blind check refuted it"));
    // Asked twice (calc.py fast finding + lib.py Loki finding), blind: never the finder's reasoning.
    let asked = got["asked"].as_array().unwrap();
    assert_eq!(asked.len(), 2);
    let calc_asked = asked
        .iter()
        .find(|item| item["state"].as_str().unwrap_or("").contains("calc.py line 5"))
        .expect("calc.py asked");
    assert_eq!(calc_asked["expect"], json!({"shown": true}));
    assert_eq!(calc_asked["kind"], "review.finding");
    let state = calc_asked["state"].as_str().unwrap();
    assert!(
        state.contains("CLAIM about calc.py line 5: mean() divides by count"),
        "{state}"
    );
    assert!(state.contains("return total(items) / count"), "{state}");
    assert!(!state.contains("SECRET-REASONING"), "{state}");
    assert_eq!(
        got["blind"],
        json!({"asked": 2, "supported": 0, "refuted": 2, "review": 0, "calls": 4, "costUsd": 0.004,
            "unpricedCalls": 2, "unavailable": null})
    );
    // Its spend is the review's: in costUsd, with the unpriced call named.
    assert_eq!(
        got["usage"],
        json!([0.004, {"calls": 4, "costUsd": 0.004, "unpricedCalls": 2}])
    );
}

#[test]
fn review_blind_check_without_a_judge_leaves_the_verdicts_and_says_why() {
    let (dir, base, head) = repository();
    let judge = "JUDGE = {'rung': 'abstained', 'settled': False, 'reason': 'no judge backend is configured (judge.provider, judge.fallbackModel)',\n\
                 \x20        'answers': {}, 'receipts': []}\n";
    let got = engine(dir.path(), &base, &head, &format!("{RLM}{judge}{RUN}"));
    let findings = got["findings"].as_array().unwrap();
    let model = findings.iter().find(|f| f[2] == "fast").expect("the model finding");
    assert_eq!((model[3].clone(), model[4].clone()), (json!("confirmed"), Value::Null));
    assert_eq!(got["blind"]["asked"], 0);
    assert_eq!(
        got["blind"]["unavailable"],
        "no judge backend is configured (judge.provider, judge.fallbackModel)"
    );
    assert_eq!(
        got["usage"],
        json!([0.0, {"calls": 0, "costUsd": 0.0, "unpricedCalls": 0}])
    );
}

struct Refuse;

impl HostHandler for Refuse {
    fn handle(&self, kind: &str, _payload: Map<String, Value>, _signal: &Signal) -> Result<Value, String> {
        Err(format!("{kind} is not available"))
    }
}

#[test]
fn review_loki_scan_reports_net_new_findings_only() {
    let handler = kernel_apis::KernelApis::new(Arc::new(Refuse)).with(Arc::new(LokiScan));
    let key = ["AKIA", "Q7XRJ2M4PL3ZWNVB"].concat();
    let old = format!("a = '{key}'\n");
    let new = format!("a = '{key}'\nb = 1\nc = '{key}'\n<<<<<<< ours\nx\n>>>>>>> theirs\n");
    let payload = json!({"changes": [["cfg.txt", old, new], ["new.txt", "", "fine\n"]]});
    let got = handler
        .handle("review.loki", payload.as_object().unwrap().clone(), &Signal::new())
        .expect("scanned");
    let violations: Vec<&str> = got["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(violations.len(), 3, "{violations:?}");
    // One more credential than the base had, and two conflict markers.
    let secrets: Vec<&&str> = violations.iter().filter(|v| v.contains(": loki/secret: ")).collect();
    assert_eq!(secrets.len(), 1, "{violations:?}");
    assert!(secrets[0].starts_with("cfg.txt:"), "{violations:?}");
    let markers = violations.iter().filter(|v| v.contains("loki/conflict-marker")).count();
    assert_eq!(markers, 2, "{violations:?}");
    for bad in [
        json!({}),
        json!({"changes": [["only-a-path"]]}),
        json!({"changes": [["", "", ""]]}),
    ] {
        let error = handler
            .handle("review.loki", bad.as_object().unwrap().clone(), &Signal::new())
            .unwrap_err();
        assert!(error.starts_with("review.loki"), "{error}");
    }
    // Everything else still goes to the inner handler.
    assert_eq!(
        handler.handle("rlm.spawn", Map::new(), &Signal::new()).unwrap_err(),
        "rlm.spawn is not available"
    );
}

/// A head commit where `_close_record` catches every exception (line 9), so `abandon_start` always releases the claim,
/// and `_write_run` treats any OSError as a missing record (line 18). (dir, base, head)
fn rechunk_repository() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("rechunk.py"), RECHUNK_BASE).expect("base");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("rechunk.py"), RECHUNK_HEAD).expect("head");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

const RECHUNK_BASE: &str = r#"import json
import os


def _close_record(path, record):
    try:
        with open(path, "w") as handle:
            json.dump(record, handle)
    finally:
        pass


def _write_run(path, record):
    try:
        with open(path) as handle:
            current = json.load(handle)
    except FileNotFoundError:
        current = None
    if current is None:
        current = record
    with open(path, "w") as handle:
        json.dump(current, handle)


def abandon_start(claims, path, record):
    _close_record(path, record)
    claims.release(record["project"])
"#;

const RECHUNK_HEAD: &str = r#"import json
import os


def _close_record(path, record):
    try:
        with open(path, "w") as handle:
            json.dump(record, handle)
    except Exception:
        return False
    return True


def _write_run(path, record):
    try:
        with open(path) as handle:
            current = json.load(handle)
    except OSError:
        current = None
    if current is None:
        current = record
    with open(path, "w") as handle:
        json.dump(current, handle)


def abandon_start(claims, path, record):
    _close_record(path, record)
    claims.release(record["project"])
"#;

/// The fast pass raises three findings; the claims investigator refutes two of them as leads. The refutation of
/// line 27 quotes line 9 as it is, so that lead is dropped; the one of line 6 quotes a line that is not there, so it
/// refutes nothing. Line 18, a different failure in the same flow, is never refuted and stays.
const RECHUNK_RLM: &str = r"LEADS = []
class Rlm:
    async def map(self, tasks, items, **options):
        task = tasks[0]
        if 'Your specialty: Correctness' in task:
            return [[
                {'file': 'rechunk.py', 'line': 27, 'end_line': 27, 'severity': 'medium', 'category': 'correctness',
                 'claim': 'abandon_start releases the project claim only after _close_record returns, so a failure in _close_record leaves the claim held.',
                 'why': 'w', 'suggested_fix': 'f', 'confidence': 0.8, 'replacement': None, 'scenario': '_close_record raises OSError'},
                {'file': 'rechunk.py', 'line': 18, 'end_line': 18, 'severity': 'medium', 'category': 'error-handling',
                 'claim': '_write_run treats any OSError reading run.json as if no record exists and overwrites the record another pod began.',
                 'why': 'w', 'suggested_fix': 'f', 'confidence': 0.8, 'replacement': None, 'scenario': 'EACCES on run.json'},
                {'file': 'rechunk.py', 'line': 6, 'end_line': 6, 'severity': 'medium', 'category': 'data-loss',
                 'claim': '_close_record truncates the record file before json.dump, so a crash mid-write leaves an empty file.',
                 'why': 'w', 'suggested_fix': 'f', 'confidence': 0.8, 'replacement': None, 'scenario': 'crash during dump'}]]
        if 'You investigate one pull request' in task:
            views = items[0]
            lead_view = next((v for v in views if isinstance(v, str) and v.startswith('Leads from the first pass')), '')
            leads = json.loads(lead_view.split('\n', 1)[1]) if lead_view else []
            LEADS.extend(leads)
            ids = {('held' if 'claim held' in l['claim'] else 'truncate' if 'truncates' in l['claim'] else 'oserror'): l.get('id') for l in leads}
            refuted = []
            if 'held' in ids:
                refuted.append({'lead': ids['held'], 'why': '_close_record catches Exception and returns, so release always runs.',
                                'evidence': [{'path': 'rechunk.py', 'line': 9, 'quote': 'except Exception:'}]})
            if 'truncate' in ids:
                # A refutation whose quote is not at its line refutes nothing.
                refuted.append({'lead': ids['truncate'], 'why': 'it writes atomically.',
                                'evidence': [{'path': 'rechunk.py', 'line': 6, 'quote': 'os.replace(tmp, path)'}]})
            return [{'findings': [], 'requests': [], 'done': True, 'refuted': refuted,
                     'checked': ['rechunk.py:9 catches Exception in _close_record, so the lead that abandon_start leaves the claim held does not hold.']}]
        if 'You check several findings' in task or 'You check one finding' in task:
            def verdict(n):
                return {'finding': n, 'verdict': 'confirmed', 'evidence': '`json.dump(current, handle)`', 'corrected_line': None,
                        'severity': 'medium', 'scenario_holds': True}
            if 'You check several findings' in task:
                return [[verdict(n) for n in range(1, 4)]]
            return [verdict(1)]
        return [[]] if 'specialty' in task else [{}]

result = asyncio.run(a.run(Rlm(), {'repoDir': repo, 'base': base, 'head': head, 'mode': 'both'}))
print(json.dumps({'findings': sorted([f['line'], f['claim'][:40]] for f in result['findings']),
                  'refuted': result['dropped'].get('refutedByInvestigator'), 'leads': [l.get('id') for l in LEADS],
                  'notChecked': result['notChecked']}))
";

/// Fisher #3239: the review posted "a failure in _close_record leaves the claim held" inline while its own summary
/// said that lead does not hold. A lead the deep pass refutes with checked evidence is not posted.
#[test]
fn review_drops_a_lead_the_deep_pass_refuted_and_keeps_a_different_failure() {
    let (dir, base, head) = rechunk_repository();
    let got = engine(dir.path(), &base, &head, RECHUNK_RLM);
    assert_eq!(
        got["findings"],
        json!([
            [6, "_close_record truncates the record file "],
            [18, "_write_run treats any OSError reading ru"]
        ]),
        "{got}"
    );
    assert_eq!(
        got["refuted"],
        json!([{"file": "rechunk.py", "line": 27, "by": "claims",
            "claim": "abandon_start releases the project claim only after _close_record returns, so a failure in \
                      _close_record leaves the claim held.",
            "why": "_close_record catches Exception and returns, so release always runs.",
            "evidence": ["rechunk.py:9"]}])
    );
    // The investigators saw each lead with the id a refutation names.
    assert!(
        got["leads"].as_array().unwrap().iter().all(serde_json::Value::is_i64),
        "{got}"
    );
}

#[test]
fn review_static_stage_emits_loki_and_ruff_nits_without_breaking_contradictions() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("mod.py"), "def run(x):\n    return x\n").expect("py base");
    std::fs::write(repo.join("app.ts"), "export function ok() { return 1; }\n").expect("ts base");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("mod.py"),
        "import os\n\ndef run(x):\n    unused = 1\n    return x\n",
    )
    .expect("py head");
    std::fs::write(
        repo.join("app.ts"),
        "export function ok() {\n  debugger;\n  try { return 1; } catch {}\n}\n",
    )
    .expect("ts head");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);

    // 1. LokiScan includes JS/TS fallback nit rules (no-debugger, no-empty) net-new.
    let handler = kernel_apis::KernelApis::new(Arc::new(Refuse)).with(Arc::new(LokiScan));
    let payload = json!({
        "changes": [
            ["app.ts", "export function ok() { return 1; }\n", "export function ok() {\n  debugger;\n  try { return 1; } catch {}\n}\n"]
        ]
    });
    let reply = handler
        .handle("review.loki", payload.as_object().unwrap().clone(), &Signal::new())
        .expect("loki fallback");
    let violations: Vec<String> = reply["violations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert!(
        violations.iter().any(|v| v.contains("no-debugger")),
        "expected no-debugger in {violations:?}"
    );
    assert!(
        violations.iter().any(|v| v.contains("no-empty")),
        "expected no-empty in {violations:?}"
    );

    // 2. autoreview_static maps Ruff F401/F841 and Loki no-debugger/no-empty to level=nit
    //    and keeps mod.py clean for lint contradictions (no error-class rules triggered).
    let body = r"
class Rlm:
    async def host_request(self, kind, payload):
        if kind == 'review.loki':
            return {'violations': ['app.ts:2: no-debugger: debugger statement', 'app.ts:3: no-empty: empty catch block swallows the error']}
        return {}

def fake_run_nits(argv, cwd, env, timeout):
    import os
    return 1, json.dumps([
        {'code': 'F401', 'message': '`os` imported but unused', 'filename': os.path.join(cwd, 'mod.py'), 'location': {'row': 1, 'column': 8}},
        {'code': 'F841', 'message': 'Local variable `unused` is assigned to but never used', 'filename': os.path.join(cwd, 'mod.py'), 'location': {'row': 4, 'column': 5}},
    ])

st.testing.run_process = fake_run_nits
scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)
out = asyncio.run(st.run_static(Rlm(), scope.files, scope.read_file, a._rev_reader(git, base)))
print(json.dumps({
    'findings': [[f['file'], f['line'], f['level'], f['severity'], f['category'], f['source']] for f in out['findings']],
    'lint_clean_for_mod': 'lint' in (out['clean'].get('mod.py') or []),
}))
";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(got["lint_clean_for_mod"], true, "{got}");
    assert_eq!(
        got["findings"],
        json!([
            ["mod.py", 1, "nit", "nit", "maintainability", "static:lint"],
            ["mod.py", 4, "nit", "nit", "maintainability", "static:lint"],
            ["app.ts", 2, "nit", "nit", "maintainability", "static:loki"],
            ["app.ts", 3, "nit", "nit", "maintainability", "static:loki"],
        ]),
        "{got}"
    );
}

#[test]
fn review_static_orphaned_private_helpers_flags_dead_helper_only() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.join("mod.py"),
        "def register(fn):\n    return fn\n\n\
         def _old_helper(x):\n    return x + 1\n\n\
         def _still_used(x):\n    return x + 2\n\n\
         @register\n\
         def _hook_cb(x):\n    return x\n\n\
         def public_fn(x):\n    return x\n\n\
         def run(x):\n    return _old_helper(x) + _still_used(x) + _hook_cb(x) + public_fn(x)\n",
    )
    .expect("base");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("mod.py"),
        "def register(fn):\n    return fn\n\n\
         def _old_helper(x):\n    return x + 1\n\n\
         def _still_used(x):\n    return x + 2\n\n\
         @register\n\
         def _hook_cb(x):\n    return x\n\n\
         def public_fn(x):\n    return x\n\n\
         def run(x):\n    return _still_used(x)\n",
    )
    .expect("head");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);

    let body = r"
scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)
found = st.orphaned_private_helpers(git, scope)
print(json.dumps([[f['file'], f['line'], f['level'], f['severity'], f['source'], f['claim']] for f in found]))
";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(
        got,
        json!([[
            "mod.py",
            4,
            "nit",
            "nit",
            "static:orphans",
            "Private helper `_old_helper` has no remaining callers after this change; delete it instead of leaving dead code."
        ]]),
        "{got}"
    );
}

#[test]
fn review_static_stage_checks_shell_and_go_syntax_net_new() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("run.sh"), "#!/usr/bin/env bash\necho ok\n").expect("sh base");
    std::fs::write(repo.join("main.go"), "package main\nfunc main() {}\n").expect("go base");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("run.sh"), "#!/usr/bin/env bash\nif true; then\n  echo oops\n").expect("sh head");
    std::fs::write(repo.join("main.go"), "package main\nfunc main() { if }\n").expect("go head");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);

    let body = r#"
def fake_which_multi(name):
    return {'bash': '/fake/bash', 'gofmt': '/fake/gofmt'}.get(name)

def fake_run_multi(argv, cwd, env, timeout):
    if argv[0] == '/fake/bash':
        if '.base' in argv[-1]:
            return 0, ''
        return 2, f"{argv[-1]}: line 3: syntax error: unexpected end of file\n"
    if argv[0] == '/fake/gofmt':
        if '.base' in argv[-1]:
            return 0, ''
        return 2, f"{argv[-1]}:2:18: expected operand, found '}}'\n"
    return 0, ''

st.shutil.which = fake_which_multi
st.testing.run_process = fake_run_multi
scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)
out = asyncio.run(st.run_static(object(), scope.files, scope.read_file, a._rev_reader(git, base)))
print(json.dumps([[f['file'], f['line'], f['level'], f['source']] for f in out['findings']]))
"#;
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(
        got,
        json!([
            ["main.go", 2, "high", "static:compile"],
            ["run.sh", 3, "high", "static:compile"],
        ]),
        "{got}"
    );
}

#[test]
fn review_static_compile_preserves_unicode_line_separators() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("oracle.py"), "TOKENS = []\n").expect("base");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("oracle.py"),
        "TOKENS = [\"\u{2028}\", \"\u{2029}\", \"\u{0085}\", \"\\x0b\", \"\\x0c\"]\n",
    )
    .expect("head");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);

    let body = r"
scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)
out = asyncio.run(st.run_static(object(), scope.files, scope.read_file, a._rev_reader(git, base), which=lambda _: None))
compile_stage = next(s for s in out['stages'] if s['name'] == 'compile')
print(json.dumps({'status': compile_stage['status'], 'findings': out['findings'], 'clean': out['clean']}))
";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(got["status"], json!("clean"), "{got}");
    assert_eq!(got["findings"], json!([]), "{got}");
    assert_eq!(got["clean"], json!({"oracle.py": ["compile"]}), "{got}");
}

#[test]
fn review_static_loki_uses_haiku_second_pass_verifier() {
    let (dir, base, head) = repository();
    let body = r"
class HaikuCheckRlm:
    def __init__(self):
        self.verify_models = []
    async def host_request(self, kind, payload):
        if kind == 'review.loki':
            return {'violations': ['lib.py:1: loki/open-redirect: unvalidated redirect']}
        raise RuntimeError(kind)
    async def map(self, tasks, items, **options):
        if 'You check one finding' in tasks[0]:
            self.verify_models.append(options.get('model'))
            return [{'verdict': 'rejected', 'evidence': '`def ok():` is not a redirect',
                     'corrected_line': None, 'severity': 'high', 'scenario_holds': False} for _ in items]
        return [[] for _ in items]

rlm = HaikuCheckRlm()
res = asyncio.run(a.run(rlm, {
    'repoDir': repo, 'base': base, 'head': head, 'mode': 'fast',
    'model': 'claude-code/sonnet', 'verifyModel': 'claude-code/sonnet',
}))
loki_findings = [f for f in res['findings'] if f.get('source') == 'static:loki']
print(json.dumps({'verify_models': rlm.verify_models, 'loki_kept': len(loki_findings)}))
";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(got["verify_models"], json!(["claude-code/haiku"]), "{got}");
    assert_eq!(got["loki_kept"], json!(0), "{got}");
}

#[test]
fn review_static_stitch_proves_regression_against_base() {
    let dir = tempfile::tempdir().expect("dir");
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(repo.join("crates/demo/src")).expect("demo");
    std::fs::create_dir_all(repo.join("stitch/demo")).expect("stitch");
    std::fs::write(
        repo.join("crates/demo/src/lib.rs"),
        "pub fn parse(s: &str) -> usize { s.len() }\n",
    )
    .expect("base lib");
    std::fs::write(
        repo.join("stitch/demo/stitch.toml"),
        "[project]\nname = \"demo\"\nlanguage = \"rust\"\nstitch_version = \"0.2.0\"\n\n[rust.dependencies]\ndemo = { path = \"/src/crates/demo\" }\n",
    )
    .expect("toml");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "base"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("crates/demo/src/lib.rs"),
        "pub fn parse(s: &str) -> usize {\n    s.as_bytes()[0] as usize\n}\n",
    )
    .expect("head lib");
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "head"]);
    let head = git(repo, &["rev-parse", "HEAD"]);

    let body = r"
scope, git, _ = a._scope({'repoDir': repo, 'base': base, 'head': head}, None)
calls = []
def fake_which(name):
    return {'docker': '/fake/docker', 'uvx': '/fake/uvx'}.get(name)
def fake_exec(argv, cwd, env, timeout):
    calls.append(argv)
    if 'crashes' in argv:
        return 0, json.dumps([
            {'id': 'c1', 'message': 'panic in crates/demo/src/lib.rs:2 index out of bounds'},
            {'id': 'c2', 'message': 'pre-existing panic in crates/demo/src/lib.rs'},
        ])
    if 'exec' in argv and 'c1' in argv:
        return 0, 'no crash'
    if 'exec' in argv and 'c2' in argv:
        return 1, 'crash: panic'
    return 0, 'ok'

out = asyncio.run(st.run_static(
    object(), scope.files, scope.read_file, a._rev_reader(git, base),
    which=fake_which, executor=fake_exec, repo_root=repo, base_rev=base,
))
stitch_stage = next(s for s in out['stages'] if s['name'] == 'stitch')
stitch_findings = [f for f in out['findings'] if f.get('source') == 'static:stitch']
print(json.dumps({
    'status': stitch_stage['status'],
    'findings': [[f['file'], f['line'], f['severity'], f['host_confirmed'], f['claim']] for f in stitch_findings],
    'calls': len(calls),
}))
";
    let got = engine(dir.path(), &base, &head, body);
    assert_eq!(got["status"], json!("findings"), "{got}");
    assert_eq!(got["findings"].as_array().map(Vec::len), Some(1), "{got}");
    assert_eq!(got["findings"][0][0], json!("crates/demo/src/lib.rs"), "{got}");
    assert_eq!(got["findings"][0][2], json!("blocker"), "{got}");
    assert_eq!(got["findings"][0][3], json!(true), "{got}");
}
