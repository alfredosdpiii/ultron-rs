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
\x20           return [{'verdict': 'confirmed', 'evidence': '`return total(items) / count`', 'corrected_line': None,\n\
\x20                    'severity': 'high', 'scenario_holds': True}]\n\
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
    // Loki's line 1 is an added line of the new file.
    assert_eq!(place("lib.py", 1, "static:loki")[3], "confirmed");
    // The verifier confirmed the model finding; the blind check (escalated) refuted it: uncertain.
    let refuted = place("calc.py", 5, "fast");
    assert_eq!(
        (refuted[3].clone(), refuted[4].clone()),
        (json!("uncertain"), json!("refuted"))
    );
    assert!(refuted[5].as_str().unwrap().starts_with("the blind check refuted it"));
    // Asked once (host findings are not asked), blind: the claim and the code, never the finder's reasoning.
    let asked = got["asked"].as_array().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["expect"], json!({"shown": true}));
    assert_eq!(asked[0]["kind"], "review.finding");
    let state = asked[0]["state"].as_str().unwrap();
    assert!(
        state.contains("CLAIM about calc.py line 5: mean() divides by count"),
        "{state}"
    );
    assert!(state.contains("return total(items) / count"), "{state}");
    assert!(!state.contains("SECRET-REASONING"), "{state}");
    assert_eq!(
        got["blind"],
        json!({"asked": 1, "supported": 0, "refuted": 1, "review": 0, "calls": 2, "costUsd": 0.002,
            "unpricedCalls": 1, "unavailable": null})
    );
    // Its spend is the review's: in costUsd, with the unpriced call named.
    assert_eq!(
        got["usage"],
        json!([0.002, {"calls": 2, "costUsd": 0.002, "unpricedCalls": 1}])
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
