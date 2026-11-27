"""Candidate verification templates, pre-plan retrieval, and planner views."""
from __future__ import annotations

import json
import re
from typing import Any, Callable

import autoreview_deep as deep
import autoreview_tests as testing
from autoreview_compiled_exec import Interpreter
from autoreview_compiled_plan import (
    CANDIDATE_READ_RADIUS,
    CITATION_READ_RADIUS,
    CLOSEST_TEST_RADIUS,
    DEPLOY_GLOBS,
    MAX_ASKS,
    MAX_EXTRA_STEPS,
    MAX_PROGRAM_STEPS,
    MAX_QUESTION_CHARS,
    MAX_STEPS,
    RETRIEVAL_CHARS,
    RETRIEVAL_HITS,
    Program,
    _flat,
    as_dict,
    as_list,
    map_legacy_severity,
    validate,
)
from infer_api import FrameError, Incomplete
from review_api import FileDiff, _clip, _text, hunk_for, render_hunk
from review_prompts import CANDIDATE_PLANNER_TASK

_COMMENTISH = re.compile(
    r"\b(comment|docstring|readme|description|documentation|help text|says|claims)\b",
    re.I,
)
_ERRORISH = re.compile(
    r"\b(except|catch|raise|throw|rollback|swallow|sentinel|exception|"
    r"error path|cleanup|finally)\b",
    re.I,
)
_INPUTISH = re.compile(
    r"\b(regex|regular expression|pattern|guard|input|validat|parse|"
    r"boundary|escape|unicode|limit)\w*",
    re.I,
)
_WORDS = re.compile(r"[A-Za-z_][A-Za-z0-9_.-]{2,}")


def _match_registry_shape(
    names: set[str],
    retrieval: dict[str, Any],
) -> tuple[str, dict[str, Any]] | None:
    registries = retrieval.get("registries")
    if not isinstance(registries, dict):
        return None
    for key in as_list(retrieval.get("keys")):
        entries = registries.get(key)
        if key in names and isinstance(entries, list) and entries:
            return "registry-member", {"key": key, "registries": entries[:3]}
    return None


def _match_env_shape(names: set[str], brief: Any) -> tuple[str, dict[str, Any]] | None:
    extracted = as_dict(getattr(brief, "extracted", None))
    env_keys = (
        as_list(extracted.get("env"))
        + as_list(extracted.get("flags"))
        + as_list(extracted.get("config"))
    )
    for name in env_keys:
        if name in names:
            return "env-in-deploy", {"name": name}
    return None


def candidate_shape(
    candidate: dict[str, Any],
    brief: Any,
    retrieval: dict[str, Any],
) -> tuple[str, dict[str, Any]]:
    """The check shape a candidate finding gets: (shape, details)."""
    if candidate.get("host_confirmed"):
        return "regression", {}
    if candidate.get("category") == "tests":
        return "unpinned-behaviour", {}

    text = f"{candidate.get('claim', '')} {candidate.get('why', '')}"
    names = set(_WORDS.findall(text))

    registry_match = _match_registry_shape(names, retrieval)
    if registry_match is not None:
        return registry_match
    env_match = _match_env_shape(names, brief)
    if env_match is not None:
        return env_match

    if candidate.get("category") == "docs" or _COMMENTISH.search(text):
        return "comment-vs-code", {}
    if _ERRORISH.search(text):
        return "error-path", {}
    if _INPUTISH.search(text):
        return "input-defeats-guard", {}
    return "consistency", {}


def _text_field(candidate: dict[str, Any], key: str, default: str = "") -> str:
    value = candidate.get(key)
    return value if value else default


def _finding_fields(candidate: dict[str, Any]) -> dict[str, Any]:
    """A candidate as the fields of a `finding` step."""
    level = candidate.get("finder_level") or candidate.get("level") or "low"
    return {
        "file": candidate["file"],
        "line": int(candidate["line"]),
        "level": level,
        "category": _text_field(candidate, "category", "correctness"),
        "claim": _text_field(candidate, "claim"),
        "why": _text_field(candidate, "why"),
        "fix": _text_field(candidate, "suggested_fix"),
        "scenario": _text_field(candidate, "scenario"),
        "citations": list(as_list(candidate.get("citations"))),
        "unpinned": candidate.get("unpinned") or None,
        "consequence": _text_field(candidate, "consequence"),
    }


def _hunk_material(candidate: dict[str, Any], files: list[FileDiff] | None) -> str | None:
    path = candidate.get("file")
    line = int(candidate.get("line") or 0)
    matched_file = next((item for item in as_list(files) if item.path == path), None)
    hunk = hunk_for(matched_file, line)
    if hunk is None:
        return None
    header = f"Diff hunk of {path} (new-file line numbers; + added, - removed):\n"
    return header + "\n".join(render_hunk(hunk))


def _retrieved_material(
    candidate: dict[str, Any],
    retrieval: dict[str, Any] | None,
) -> str | None:
    sections = as_dict(retrieval).get("sections")
    if not isinstance(sections, dict):
        return None
    text = f"{candidate.get('claim', '')} {candidate.get('why', '')}"
    names = set(_WORDS.findall(text))
    used = [sections[name] for name in sections if name in names][:2]
    if not used:
        return None
    header = (
        "Retrieved references of the names the claim uses "
        "(outside the changed lines, at the reviewed commit):\n\n"
    )
    return header + "\n\n".join(used)


def ask_material(
    candidate: dict[str, Any],
    files: list[FileDiff] | None,
    retrieval: dict[str, Any] | None,
) -> list[str]:
    """The diff hunk and retrieved references attached to an ask about a candidate."""
    out: list[str] = []
    hunk_text = _hunk_material(candidate, files)
    if hunk_text is not None:
        out.append(hunk_text)
    retrieved_text = _retrieved_material(candidate, retrieval)
    if retrieved_text is not None:
        out.append(retrieved_text)
    return out


def _append_read_step(
    steps: list[dict[str, Any]],
    repo: deep.Repo,
    sid: str,
    target: str,
    at: int,
    radius: int,
) -> bool:
    total = len(as_list(repo.lines(target)))
    if not total:
        return False
    start = max(1, at - radius)
    end = min(total, at + radius)
    steps.append(
        {"id": sid, "op": "read", "args": {"path": target, "start": start, "end": end}}
    )
    return True


def _make_finding_step(
    cid: str,
    gate: str,
    evidence: list[str],
    fields: dict[str, Any],
    negate: bool = False,
) -> dict[str, Any]:
    return {
        "id": f"{cid}_f",
        "op": "finding",
        "when": {"step": gate, "not": negate},
        "evidence": evidence,
        **fields,
    }


def _template_unpinned(
    cid: str,
    candidate: dict[str, Any],
    fields: dict[str, Any],
    material: list[str],
    *,
    repo: deep.Repo,
    brief: Any,
    tests_available: bool,
    runnable: Callable[[list[str]], bool] | None,
) -> tuple[list[dict[str, Any]], str]:
    steps: list[dict[str, Any]] = []
    unpinned = as_dict(candidate.get("unpinned"))
    mutation = unpinned.get("mutation")
    closest = unpinned.get("closest_test")
    has_closest = isinstance(closest, dict) and closest.get("path") in repo.files()
    test_paths = [closest["path"]] if has_closest else list(getattr(brief, "tests", [])[:2])

    can_mutate = (
        bool(mutation)
        and tests_available
        and bool(test_paths)
        and (runnable is None or runnable(test_paths))
    )
    if can_mutate:
        steps.append(
            {
                "id": f"{cid}_m",
                "op": "mutation_check",
                "args": {
                    "path": mutation["path"],
                    "line": mutation["line"],
                    "replacement": mutation["replacement"],
                    "tests": test_paths,
                },
            }
        )
        steps.append(
            {
                "id": f"{cid}_a",
                "op": "assert",
                "step": f"{cid}_m",
                "predicate": "status == passed",
                "expect": True,
                "holds": f"nothing pins {mutation['path']}:{mutation['line']}: the mutant survived",
            }
        )
        steps.append(_make_finding_step(cid, f"{cid}_a", [f"{cid}_m"], fields))
        desc = (
            f"mutation_check of {mutation['path']}:{mutation['line']} "
            f"against {', '.join(test_paths)}"
        )
        return steps, desc

    return _template_unpinned_ask(cid, fields, material, unpinned, closest, test_paths, repo)


def _collect_unpinned_context(
    cid: str,
    fields: dict[str, Any],
    closest: Any,
    test_path: str | None,
    repo: deep.Repo,
) -> tuple[list[dict[str, Any]], list[str]]:
    steps: list[dict[str, Any]] = []
    context: list[str] = []
    if test_path:
        at = int(closest.get("line") or 1) if isinstance(closest, dict) else 1
        if _append_read_step(steps, repo, f"{cid}_t", test_path, at, CLOSEST_TEST_RADIUS):
            context.append(f"{cid}_t")
    read_ok = _append_read_step(
        steps, repo, f"{cid}_r", fields["file"], fields["line"], CANDIDATE_READ_RADIUS
    )
    if read_ok:
        context.append(f"{cid}_r")
    return steps, context


def _template_unpinned_ask(
    cid: str,
    fields: dict[str, Any],
    material: list[str],
    unpinned: dict[str, Any],
    closest: Any,
    test_paths: list[str],
    repo: deep.Repo,
) -> tuple[list[dict[str, Any]], str]:
    test_path = test_paths[0] if test_paths else None
    steps, context = _collect_unpinned_context(cid, fields, closest, test_path, repo)
    if not context:
        return [], "no source to read"

    behaviour = unpinned.get("behaviour") or fields["claim"]
    change = unpinned.get("change") or "undo the new behaviour"
    question = (
        f"Behaviour: {behaviour}. Change to it: {change}. "
        "Looking at the test lines shown, would every assertion still pass after that change "
        "(that is, does no shown test pin this behaviour)? Say first which shown assertion would "
        "fail if one would. Answer yes if the behaviour is unpinned, quoting the closest "
        "assertion that still passes; no if a shown assertion would fail, quoting it; unclear "
        "if the lines cannot tell."
    )
    steps.append(
        {
            "id": f"{cid}_q",
            "op": "ask",
            "question": question,
            "context": context,
            "material": material,
        }
    )
    steps.append(_make_finding_step(cid, f"{cid}_q", [f"{cid}_q", *context], fields))
    return steps, f"ask over {test_path or fields['file']} whether the change would be caught"


def _template_registry_member(
    cid: str,
    details: dict[str, Any],
    fields: dict[str, Any],
) -> tuple[list[dict[str, Any]], str]:
    key = details["key"]
    registries = details["registries"][:3]
    steps: list[dict[str, Any]] = []
    asserts: list[str] = []
    for number, registry in enumerate(registries, 1):
        steps.append(
            {
                "id": f"{cid}_g{number}",
                "op": "grep",
                "args": {"pattern": re.escape(key), "path_glob": registry, "count_only": True},
            }
        )
        steps.append(
            {
                "id": f"{cid}_a{number}",
                "op": "assert",
                "step": f"{cid}_g{number}",
                "predicate": "count == 0",
                "expect": True,
                "holds": f"`{key}` is registered in {registry}",
            }
        )
        asserts.append(f"{cid}_a{number}")

    gate = asserts[0]
    if len(asserts) > 1:
        steps.append({"id": f"{cid}_all", "op": "assert", "any": asserts, "expect": True})
        gate = f"{cid}_all"
    evidence = [f"{cid}_g{number}" for number in range(1, len(asserts) + 1)]
    steps.append(_make_finding_step(cid, gate, evidence, fields))
    return steps, f"count_only grep of `{key}` in {', '.join(registries)}"


def _template_env_in_deploy(
    cid: str,
    details: dict[str, Any],
    fields: dict[str, Any],
) -> tuple[list[dict[str, Any]], str]:
    name = details["name"]
    globs = DEPLOY_GLOBS[:4]
    steps: list[dict[str, Any]] = []
    asserts: list[str] = []
    for number, glob in enumerate(globs, 1):
        steps.append(
            {
                "id": f"{cid}_g{number}",
                "op": "grep",
                "args": {"pattern": re.escape(name), "path_glob": glob, "count_only": True},
            }
        )
        steps.append(
            {
                "id": f"{cid}_a{number}",
                "op": "assert",
                "step": f"{cid}_g{number}",
                "predicate": "count == 0",
                "expect": True,
            }
        )
        asserts.append(f"{cid}_a{number}")

    holds_msg = f"`{name}` is set in no manifest, compose file, workflow or infrastructure file"
    steps.append(
        {"id": f"{cid}_all", "op": "assert", "all": asserts, "expect": True, "holds": holds_msg}
    )
    evidence = [f"{cid}_g{number}" for number in range(1, len(asserts) + 1)]
    steps.append(_make_finding_step(cid, f"{cid}_all", evidence, fields))
    return steps, f"count_only grep of `{name}` in {', '.join(globs)}"


def _material_description(material: list[str]) -> str:
    if len(material) == 2:
        return ", the diff hunk and retrieved references attached"
    if material and material[0].startswith("Diff hunk"):
        return ", the diff hunk attached"
    if material:
        return ", retrieved references attached"
    return ""


def _collect_general_context(
    cid: str,
    candidate: dict[str, Any],
    path: str,
    line: int,
    repo: deep.Repo,
) -> tuple[list[dict[str, Any]], list[str]]:
    steps: list[dict[str, Any]] = []
    context: list[str] = []
    if _append_read_step(steps, repo, f"{cid}_r", path, line, CANDIDATE_READ_RADIUS):
        context.append(f"{cid}_r")

    for number, citation in enumerate(as_list(candidate.get("citations"))[:3], 1):
        cit_line = int(citation.get("line") or 0)
        if citation.get("path") == path and abs(cit_line - line) <= CANDIDATE_READ_RADIUS:
            continue
        read_ok = _append_read_step(
            steps, repo, f"{cid}_c{number}", citation["path"], cit_line, CITATION_READ_RADIUS
        )
        if read_ok:
            context.append(f"{cid}_c{number}")
    return steps, context


def _template_general_ask(
    cid: str,
    candidate: dict[str, Any],
    fields: dict[str, Any],
    material: list[str],
    repo: deep.Repo,
) -> tuple[list[dict[str, Any]], str]:
    path, line = fields["file"], fields["line"]
    steps, context = _collect_general_context(cid, candidate, path, line, repo)
    if not context:
        return [], "no source to read"

    scenario_part = f" Scenario: {fields['scenario']}" if fields["scenario"] else ""
    question = (
        f"Candidate finding at {path}:{line}: {fields['claim']} "
        f"Reason given: {fields['why'] or '(none)'}{scenario_part} "
        "Two sides: first, what would the material have to show for this claim to be false "
        "(a guard, a handler, a caller that validates, a test that pins it, a different order)? "
        "Is that there? Then answer yes only if the cited lines have the problem as described "
        "and nothing shown prevents it, quoting the cited line that has it; no if the code "
        "shown prevents it or the claim misreads the code, quoting the line that does; unclear "
        "if the material cannot tell."
    )
    steps.append(
        {
            "id": f"{cid}_q",
            "op": "ask",
            "question": _clip(question, MAX_QUESTION_CHARS),
            "context": context,
            "material": material,
        }
    )
    steps.append(_make_finding_step(cid, f"{cid}_q", [f"{cid}_q", *context], fields))
    desc = (
        f"ask over {', '.join(context)} with the claim, reason and scenario"
        f"{_material_description(material)}"
    )
    return steps, desc


def template_steps(
    cid: str,
    candidate: dict[str, Any],
    shape: str,
    details: dict[str, Any],
    *,
    repo: deep.Repo,
    brief: Any,
    tests_available: bool,
    files: list[FileDiff] | None = None,
    retrieval: dict[str, Any] | None = None,
    runnable: Callable[[list[str]], bool] | None = None,
) -> tuple[list[dict[str, Any]], str]:
    """The check program of one candidate, from its shape: (steps, what the template does)."""
    fields = _finding_fields(candidate)
    material = ask_material(candidate, files, retrieval)
    if shape == "unpinned-behaviour":
        return _template_unpinned(
            cid,
            candidate,
            fields,
            material,
            repo=repo,
            brief=brief,
            tests_available=tests_available,
            runnable=runnable,
        )
    if shape == "registry-member":
        return _template_registry_member(cid, details, fields)
    if shape == "env-in-deploy":
        return _template_env_in_deploy(cid, details, fields)
    return _template_general_ask(cid, candidate, fields, material, repo)


def _quote_in(
    repo: deep.Repo,
    path: str,
    line: int,
    quote: str,
    radius: int = CANDIDATE_READ_RADIUS,
) -> bool:
    lines = as_list(repo.lines(path))
    flat = _flat(quote)
    if len(flat) < 4:
        return False
    start = max(1, line - radius)
    end = min(len(lines), line + radius)
    for number in range(start, end + 1):
        flat_line = _flat(lines[number - 1])
        if flat in flat_line or (len(flat_line) >= 8 and flat_line in flat):
            return True
    return False


def _quote_in_reads(repo: deep.Repo, steps: list[dict[str, Any]], quote: str) -> bool:
    """Whether a quote comes from the lines a check program read."""
    for step in steps:
        if step.get("op") != "read" or not isinstance(step.get("args"), dict):
            continue
        args = step["args"]
        start, end = int(args.get("start") or 1), int(args.get("end") or 0)
        middle = (start + end) // 2
        span = max(1, (end - start) // 2 + 1)
        if _quote_in(repo, str(args.get("path")), middle, quote, radius=span):
            return True
    return False


def _make_runnable_checker(
    session: Any,
    unavailable: set[tuple[str, str]] | None,
) -> Callable[[list[str]], bool]:
    known_unavailable = set(unavailable) if unavailable else set()

    def runnable(test_paths: list[str]) -> bool:
        if session is None or not known_unavailable:
            return session is not None
        try:
            groups = session.plan(session.paths(test_paths))
        except testing.TestsRejected:
            return True
        return not any(
            (runner.name, directory or ".") in known_unavailable
            for runner, directory, _paths in groups
        )

    return runnable


def _build_candidate_plans(
    candidates: list[dict[str, Any]],
    *,
    batch: str,
    limit: int,
    start_index: int,
    repo: deep.Repo,
    brief: Any,
    retrieval: dict[str, Any],
    tests_available: bool,
    files: list[FileDiff] | None,
    runnable: Callable[[list[str]], bool],
) -> tuple[list[tuple[str, dict[str, Any], str, str]], list[str]]:
    plans: list[tuple[str, dict[str, Any], str, str]] = []
    skipped: list[str] = []
    for index, candidate in enumerate(candidates[:limit], start_index + 1):
        cid = f"c{index}"
        candidate["verification"] = {"state": "pending", "candidate": cid, "batch": batch}
        if candidate.get("host_confirmed"):
            candidate["verification"] = {
                "state": "confirmed",
                "candidate": cid,
                "batch": batch,
                "shape": "regression",
                "template": "the host ran the test at both commits",
                "by": f"test:{candidate.get('test_run')}",
            }
            continue

        shape, details = candidate_shape(candidate, brief, retrieval)
        steps, how = template_steps(
            cid,
            candidate,
            shape,
            details,
            repo=repo,
            brief=brief,
            tests_available=tests_available,
            files=files,
            retrieval=retrieval,
            runnable=runnable,
        )
        candidate["verification"].update(shape=shape, template=how)
        if not steps:
            candidate["verification"].update(state="unknown", detail=how)
            skipped.append(f"{cid}: {how}")
            continue
        plans.append((cid, candidate, shape, how))
        candidate["_steps"] = steps

    for candidate in candidates[limit:]:
        candidate["verification"] = {
            "state": "unknown",
            "batch": batch,
            "detail": "beyond the candidate cap",
        }
    return plans, skipped


def _apply_planner_extra_steps(
    plans: list[tuple[str, dict[str, Any], str, str]],
    extra: dict[str, Any],
    planner: dict[str, Any],
) -> None:
    for cid, candidate, _shape, _how in plans:
        added = extra.get(cid)
        if not isinstance(added, list) or not added:
            continue
        valid_added = [
            item
            for item in added[:MAX_EXTRA_STEPS]
            if isinstance(item, dict)
            and isinstance(item.get("id"), str)
            and item["id"].startswith(f"{cid}_")
        ]
        trial = candidate["_steps"] + valid_added
        if len(trial) > len(candidate["_steps"]) and validate({"steps": trial})[0] is not None:
            added_count = len(trial) - len(candidate["_steps"])
            planner["extra"] += added_count
            candidate["verification"]["extra"] = added_count
            candidate["_steps"] = trial
        else:
            candidate["verification"]["extra_rejected"] = True


def _build_candidate_planner_views(
    plans: list[tuple[str, dict[str, Any], str, str]],
    diff_text: str,
    retrieval: dict[str, Any],
) -> list[str]:
    views = [deep.diff_view(diff_text)]
    if retrieval.get("text"):
        views.append("Retrieved context (untrusted repository data):\n" + retrieval["text"])

    shown = []
    for cid, candidate, shape, how in plans:
        public = {
            key: candidate.get(key)
            for key in ("file", "line", "level", "category", "claim", "why", "scenario")
        }
        shown.append(
            f"Candidate {cid} (shape {shape}; template: {how}):\n"
            f"{json.dumps(public, indent=1)}\nTemplate program:\n"
            + json.dumps(candidate["_steps"], indent=1)
        )
    views.append("Candidates and their template programs:\n\n" + "\n\n".join(shown))
    return views


def _sum_plan_tokens(frames: Any) -> int:
    return sum(
        int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan"
    )


def _reply_error_text(reply: Any) -> str:
    error = getattr(reply, "error", None)
    if error:
        return _text(error, 160)
    status = getattr(reply, "status", None)
    return _text(status if status else "no reply", 160)


async def _run_candidate_planner(
    plans: list[tuple[str, dict[str, Any], str, str]],
    *,
    diff_text: str,
    retrieval: dict[str, Any],
    frames: Any,
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
) -> dict[str, Any]:
    planner: dict[str, Any] = {"status": "skipped", "extra": 0, "ms": 0, "tokens": 0}
    views = _build_candidate_planner_views(plans, diff_text, retrieval)
    before = _sum_plan_tokens(frames)
    planner_began = clock()
    replies = await frames.run(
        "plan",
        [("candidate-planner", CANDIDATE_PLANNER_TASK, views)],
        contract={
            "type": "object",
            "properties": {"extra": {"type": "object"}},
            "required": ["extra"],
        },
        model=plan_model,
        thinking=plan_thinking,
        cutoff=cutoff,
    )
    planner["ms"] = int((clock() - planner_began) * 1000)
    planner["tokens"] = _sum_plan_tokens(frames) - before

    reply = replies[0]
    if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
        planner["status"] = "failed"
        planner["error"] = _reply_error_text(reply)
        return planner

    planner["status"] = "ok"
    _apply_planner_extra_steps(plans, as_dict(reply.get("extra")), planner)
    return planner


def _validate_candidate_program(
    plans: list[tuple[str, dict[str, Any], str, str]],
) -> Program:
    all_steps = [step for _cid, candidate, _shape, _how in plans for step in candidate["_steps"]]
    program, errors = validate({"steps": all_steps}) if all_steps else (Program([], ""), [])
    if program is not None:
        return program

    kept: list[dict[str, Any]] = []
    for _cid, candidate, _shape, _how in plans:
        if validate({"steps": candidate["_steps"]})[0] is not None:
            kept.extend(candidate["_steps"])
        else:
            candidate["verification"].update(
                state="unknown",
                detail="the check program did not validate: " + "; ".join(errors[:2]),
            )
    validated, _errors = validate({"steps": kept}) if kept else (Program([], ""), [])
    return validated or Program([], "")


def _extract_ask_quote(ask_evidence: str) -> str:
    quote = ""
    for line in ask_evidence.split("\n"):
        if "quoting `" in line:
            quote = line.split("quoting `", 1)[1].rsplit("`", 1)[0]
    return quote


def _apply_emitted_candidate(
    candidate: dict[str, Any],
    emitted: dict[str, Any],
    steps: list[dict[str, Any]],
    gate: str,
    repo: deep.Repo,
) -> bool:
    """Apply an emitted finding; return True if capped for quoting outside cited lines."""
    state = candidate["verification"]
    asked = not emitted.get("host_confirmed")
    if asked:
        quote = _extract_ask_quote(emitted.get("ask_evidence", ""))
        if not _quote_in_reads(repo, steps, quote):
            detail = (
                "the small model answered yes but quoted outside the cited lines "
                "(left to the verifier)"
            )
            state.update(state="unknown", gate=gate or "gate true", detail=detail)
            return True

    level = emitted["level"]
    candidate.update(
        {
            "level": level,
            "severity": map_legacy_severity(level),
            "evidence": emitted.get("evidence", ""),
            "beyond_diff": emitted.get("beyond_diff") or candidate.get("beyond_diff"),
            "verified_by": f"check:{emitted['program_step']}",
        }
    )
    for key in (
        "host_confirmed",
        "how_verified",
        "test_run",
        "test_evidence",
        "ask_evidence",
        "proof",
    ):
        if emitted.get(key) is not None:
            candidate[key] = emitted[key]

    if asked:
        ask_ev = candidate.get("ask_evidence") or "a small model answered yes over the cited lines"
        candidate["how_verified"] = f"check {emitted['program_step']}: " + _clip(ask_ev, 300)
        candidate.pop("host_confirmed", None)

    state.update(
        state="confirmed",
        by=f"check:{emitted['program_step']}",
        asked=asked,
        gate=gate or "finding emitted",
    )
    return False


def _is_refuted_record(record: dict[str, Any], gate: str) -> bool:
    if record.get("status") != "ok" or record.get("emitted") is not False:
        return False
    return gate.startswith("gate false") or gate.endswith("ask: no") or "refuted" in gate


def _record_ask_state(state: dict[str, Any], ask: dict[str, Any] | None) -> None:
    if ask is None or ask.get("op") != "ask" or ask.get("status") != "ok":
        return
    state["asked"] = {
        "answer": ask.get("answer"),
        "quote": _clip(ask.get("quote") or "", 200),
        "why": ask.get("why") or "",
    }


def _classify_candidate_outcome(
    candidate: dict[str, Any],
    steps: list[dict[str, Any]],
    record: dict[str, Any] | None,
    emitted: dict[str, Any] | None,
    repo: deep.Repo,
) -> bool:
    state = candidate["verification"]
    gate = as_dict(record).get("gate") or ""
    if emitted is not None:
        return _apply_emitted_candidate(candidate, emitted, steps, gate, repo)
    if record is None:
        state.update(state="unknown", detail="the check program did not reach the finding step")
        return False
    if _is_refuted_record(record, gate):
        state.update(state="refuted", gate=gate, detail=record.get("detail", ""))
        return False
    if gate.startswith("gate true; dropped"):
        state.update(state="dropped", gate=gate, detail=record.get("detail", ""))
        return False
    state.update(
        state="unknown",
        gate=gate or record.get("detail", ""),
        detail=record.get("detail", ""),
    )
    return False


def _apply_candidate_results(
    plans: list[tuple[str, dict[str, Any], str, str]],
    interpreter: Interpreter,
    repo: deep.Repo,
) -> int:
    composed = {finding["program_step"]: finding for finding in interpreter.findings}
    results = interpreter.results
    capped = 0
    for cid, candidate, _shape, _how in plans:
        steps = as_list(candidate.pop("_steps", None))
        state = candidate["verification"]
        if state["state"] != "pending":
            continue
        _record_ask_state(state, results.get(f"{cid}_q"))
        record = results.get(f"{cid}_f")
        emitted = composed.get(f"{cid}_f")
        if _classify_candidate_outcome(candidate, steps, record, emitted, repo):
            capped += 1
    return capped


def _count_verification_states(
    candidates: list[dict[str, Any]],
) -> tuple[dict[str, int], dict[str, int]]:
    counts = {"confirmed": 0, "refuted": 0, "unknown": 0, "dropped": 0}
    shapes: dict[str, int] = {}
    for candidate in candidates:
        state = candidate.get("verification", {})
        label = state.get("state", "unknown")
        counts[label] = counts.get(label, 0) + 1
        shape = state.get("shape")
        if shape:
            shapes[shape] = shapes.get(shape, 0) + 1
    return counts, shapes


async def verify_candidates(
    candidates: list[dict[str, Any]],
    *,
    batch: str,
    repo: deep.Repo,
    brief: Any,
    retrieval: dict[str, Any],
    frames: Any,
    session: Any,
    diff_lines: dict[str, set[int]],
    diff_text: str,
    plan_model: str | None,
    plan_thinking: str | None,
    ask_model: str | None,
    ask_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    cap: Callable[..., str],
    to_level: Callable[[Any], str | None] | None,
    enrich: Callable[[Any, dict[str, Any]], None] | None,
    generic: Callable[[dict[str, Any]], str | None] | None,
    unavailable: set[tuple[str, str]] | None,
    limit: int,
    use_planner: bool = True,
    start_index: int = 0,
    files: list[FileDiff] | None = None,
) -> dict[str, Any]:
    """Decide a batch of candidate findings with host-written check programs."""
    began = clock()
    tests_available = session is not None and session.limit > len(session.records)
    runnable = _make_runnable_checker(session, unavailable)
    plans, skipped = _build_candidate_plans(
        candidates,
        batch=batch,
        limit=limit,
        start_index=start_index,
        repo=repo,
        brief=brief,
        retrieval=retrieval,
        tests_available=tests_available,
        files=files,
        runnable=runnable,
    )

    planner: dict[str, Any] = {"status": "skipped", "extra": 0, "ms": 0, "tokens": 0}
    if plans and use_planner:
        planner = await _run_candidate_planner(
            plans,
            diff_text=diff_text,
            retrieval=retrieval,
            frames=frames,
            plan_model=plan_model,
            plan_thinking=plan_thinking,
            cutoff=cutoff,
            clock=clock,
        )

    program = _validate_candidate_program(plans)
    interpreter = Interpreter(
        program,
        repo,
        frames=frames,
        session=session,
        diff_lines=diff_lines,
        ask_model=ask_model,
        ask_thinking=ask_thinking,
        cutoff=cutoff,
        clock=clock,
        cap=cap,
        to_level=to_level,
        enrich=enrich,
        generic=generic,
        unavailable=unavailable,
        max_steps=MAX_STEPS,
        defer_unrunnable=True,
    )
    if program.steps:
        await interpreter.run()

    capped = _apply_candidate_results(plans, interpreter, repo)
    counts, shapes = _count_verification_states(candidates)
    return {
        "batch": batch,
        "candidates": len(candidates),
        "checked": len(plans),
        **counts,
        "capped": capped,
        "shapes": shapes,
        "skipped": skipped,
        "planner": planner,
        "records": interpreter.records,
        "stats": interpreter.stats(),
        "assurance": interpreter.assurance(),
        "ms": int((clock() - began) * 1000),
    }


def runner_availability(session: Any) -> tuple[list[str], set[tuple[str, str]]]:
    """What the automatic run showed per runner and directory."""
    if session is None:
        return [], set()

    seen: dict[tuple[str, str], str] = {}
    for record in session.records:
        key = (record["runner"], record.get("cwd") or ".")
        if key not in seen or record["status"] != "passed":
            seen[key] = record["status"]

    lines: list[str] = []
    unavailable: set[tuple[str, str]] = set()
    for (runner, directory), status in seen.items():
        where = f"{runner} in {directory}"
        if status == "unavailable":
            unavailable.add((runner, directory))
            lines.append(
                f"{where}: unavailable (missing dependencies; run_tests and mutation_check on "
                "its files will not run: read the tests and ask instead)"
            )
        else:
            lines.append(f"{where}: available (the automatic run {status})")

    if not lines and session.runner is not None:
        runner_info = f"{session.runner.name} (the repository's runner, {session.runner.because})"
        lines.append(f"{runner_info}: not tried yet")
    return lines, unavailable


def _filter_outside(
    hits: list[tuple[str, int, str]],
    changed_lines: dict[str, set[int]],
) -> list[tuple[str, int, str]]:
    return [hit for hit in hits if hit[1] not in changed_lines.get(hit[0], set())]


def _symbol_hits(
    repo: deep.Repo,
    name: str,
    changed_lines: dict[str, set[int]],
) -> list[tuple[str, int, str]]:
    hits = _filter_outside(repo.grep(name, fixed=True, word=True, limit=60), changed_lines)
    reference = getattr(repo, "reference", None)
    if reference is not None and reference.symbols_named(name):
        calls = [(call.path, call.line, call.text) for call in reference.callers(name)]
        if calls:
            return calls + [hit for hit in hits if hit[0] not in reference.index]
    return hits


def _append_test_mentions(
    repo: deep.Repo,
    tests: list[tuple[str, int, str]],
    lines: list[str],
) -> None:
    test_paths = list(dict.fromkeys(hit[0] for hit in tests))[:3]
    for test_path in test_paths:
        mentions = [
            f"{line_no}: {_clip(text.strip(), 120)}"
            for path, line_no, text in tests
            if path == test_path
        ][:4]
        lines.append(f"  test {test_path} mentions it at " + "; ".join(mentions))
        structure = repo.grep(deep._STRUCTURE, pathspec=test_path, limit=6)
        if structure:
            summary = "; ".join(
                f"{line_no}: {_clip(text.strip(), 100)}" for _, line_no, text in structure
            )
            lines.append("    structure: " + summary)


def _format_symbol_section(
    repo: deep.Repo,
    name: str,
    hits: list[tuple[str, int, str]],
) -> tuple[int, str]:
    if not hits:
        return 0, f"References of `{name}` outside the changed lines: none."

    tests = [hit for hit in hits if deep._TEST.search(hit[0])]
    code = [hit for hit in hits if hit not in tests]
    lines = [
        f"References of `{name}` outside the changed lines "
        f"({len(hits)} in all, {len(tests)} in tests):"
    ]
    for path, line_no, text in code[:RETRIEVAL_HITS]:
        lines.append(f"  {path}:{line_no}: {_clip(text.strip(), 160)}")
    if len(code) > RETRIEVAL_HITS:
        lines.append(f"  ... {len(code) - RETRIEVAL_HITS} more")
    _append_test_mentions(repo, tests, lines)
    return len(hits), "\n".join(lines)


def _collect_declared_fields(repo: deep.Repo, files: list[FileDiff]) -> list[str]:
    declared: list[str] = []
    for item in files:
        for line in as_list(repo.lines(item.path)):
            match = deep._FIELD.match(line)
            if match and match.group(1) not in declared:
                declared.append(match.group(1))
    return declared


def _find_sibling_files(
    family: list[tuple[str, int, str]],
    siblings: list[str],
) -> dict[str, set[str]]:
    by_file: dict[str, set[str]] = {}
    for path, _line_no, text in family:
        matched = {
            other
            for other in siblings
            if re.search(r"(?<![A-Za-z0-9_])" + re.escape(other) + r"(?![A-Za-z0-9_])", text)
        }
        by_file.setdefault(path, set()).update(matched)
    return by_file


def _format_key_section(
    key: str,
    own: list[tuple[str, int, str]],
    found: list[str],
    by_file: dict[str, set[str]],
    family: list[tuple[str, int, str]],
    present: set[str],
) -> str:
    own_hits = "; ".join(f"{path}:{line_no}" for path, line_no, _text in own[:RETRIEVAL_HITS])
    own_text = own_hits if own_hits else "nowhere"
    lines = [f"New key `{key}`: used outside the changed lines at {own_text}."]
    for path in found[:5]:
        where = "; ".join(
            f"{line_no}: {_clip(text.strip(), 100)}"
            for fam_path, line_no, text in family
            if fam_path == path
        )[:400]
        presence = (
            ": the new key is NOT there" if path not in present else ": the new key is there too"
        )
        lines.append(f"  siblings ({len(by_file[path])}) registered in {path}{presence} ({where})")
    return "\n".join(lines)


def _family_hits_outside(
    repo: deep.Repo,
    siblings: list[str],
    changed_paths: set[str],
    changed_lines: dict[str, set[int]],
) -> list[tuple[str, int, str]]:
    if not siblings:
        return []
    pattern = "|".join(re.escape(other) for other in siblings)
    return [
        hit
        for hit in repo.grep(pattern, limit=80)
        if hit[0] not in changed_paths or hit[1] not in changed_lines.get(hit[0], set())
    ]


def _retrieve_one_key(
    repo: deep.Repo,
    key: str,
    keys: list[str],
    declared: list[str],
    changed_paths: set[str],
    changed_lines: dict[str, set[int]],
) -> tuple[list[str], list[str], int, str]:
    own = _filter_outside(repo.grep(key, fixed=True, word=True, limit=40), changed_lines)
    siblings = [other for other in list(dict.fromkeys(keys + declared)) if other != key][:12]
    family = _family_hits_outside(repo, siblings, changed_paths, changed_lines)
    by_file = _find_sibling_files(family, siblings)
    present = {
        path for path, _line_no, _text in repo.grep(key, fixed=True, word=True, limit=60)
    }
    found = [
        path
        for path, names in sorted(by_file.items(), key=lambda pair: -len(pair[1]))
        if len(names) >= 2
    ]
    missing = [path for path in found if path not in present]
    body = _format_key_section(key, own, found, by_file, family, present)
    return missing, sorted(present), 100 + len(found) * 10, body


def _retrieve_key_sections(
    repo: deep.Repo,
    keys: list[str],
    declared: list[str],
    changed_paths: set[str],
    changed_lines: dict[str, set[int]],
    sections: list[tuple[int, str]],
    by_name: dict[str, str],
) -> tuple[dict[str, list[str]], dict[str, list[str]], int]:
    registries: dict[str, list[str]] = {}
    present_in: dict[str, list[str]] = {}
    for key in keys:
        missing, present_sorted, rank, body = _retrieve_one_key(
            repo, key, keys, declared, changed_paths, changed_lines
        )
        if missing:
            registries[key] = missing
        present_in[key] = present_sorted
        sections.append((rank, body))
        by_name[key] = body
    return registries, present_in, len(keys)


def _retrieve_call_sections(
    repo: deep.Repo,
    brief: Any,
    extracted: dict[str, Any],
    sections: list[tuple[int, str]],
) -> int:
    helpers = as_list(getattr(brief, "helpers", None))
    added_items = 0
    for name in as_list(extracted.get("calls"))[:5]:
        if name in helpers:
            continue
        hits = repo.grep(deep._definition_pattern(name), limit=2)
        if not hits:
            continue
        path, line_no, _text = hits[0]
        lines = repo.lines(path)
        if not lines:
            continue
        snippet = deep._numbered(lines, line_no, min(len(lines), line_no + 11))
        entry = f"Called by the change, `{name}` is defined at {path}:{line_no}:\n{snippet}"
        sections.append((1, entry))
        added_items += 1
    return added_items


def _join_ranked_sections(sections: list[tuple[int, str]], limit: int) -> str:
    text = ""
    for _rank, section in sorted(sections, key=lambda pair: -pair[0]):
        if len(text) + len(section) + 2 > limit:
            text += "\n... (retrieved context cut at its size limit)"
            break
        text += ("\n\n" if text else "") + section
    return text


def _collect_added_lines(files: list[FileDiff]) -> dict[str, set[int]]:
    return {
        item.path: {
            line.new
            for hunk in item.hunks
            for line in hunk.lines
            if line.kind == "+" and line.new
        }
        for item in files
    }


def _retrieve_symbol_sections(
    repo: deep.Repo,
    brief: Any,
    changed_lines: dict[str, set[int]],
    sections: list[tuple[int, str]],
    by_name: dict[str, str],
) -> int:
    symbols = as_list(getattr(brief, "symbols", None))[:8]
    for name in symbols:
        hits = _symbol_hits(repo, name, changed_lines)
        rank, section = _format_symbol_section(repo, name, hits)
        sections.append((rank, section))
        by_name[name] = section
    return len(symbols)


def retrieve(
    repo: deep.Repo,
    brief: Any,
    files: list[FileDiff],
    *,
    clock: Callable[[], float],
    limit: int = RETRIEVAL_CHARS,
) -> dict[str, Any]:
    """Mechanical lookups done by the host before the planner runs."""
    began = clock()
    changed_lines = _collect_added_lines(files)
    changed_paths = {item.path for item in files}
    extracted = as_dict(getattr(brief, "extracted", None))
    required = as_dict(getattr(brief, "required", None))

    sections: list[tuple[int, str]] = []
    by_name: dict[str, str] = {}
    items = _retrieve_symbol_sections(repo, brief, changed_lines, sections, by_name)

    raw_keys = (
        as_list(required.get("claims"))
        + as_list(extracted.get("fields"))
        + as_list(extracted.get("constants"))
    )
    keys = list(dict.fromkeys(raw_keys))[:8]
    declared = _collect_declared_fields(repo, files)
    registries, present_in, key_items = _retrieve_key_sections(
        repo, keys, declared, changed_paths, changed_lines, sections, by_name
    )
    items += key_items + _retrieve_call_sections(repo, brief, extracted, sections)

    text = _join_ranked_sections(sections, limit)
    return {
        "text": text,
        "items": items,
        "chars": len(text),
        "ms": int((clock() - began) * 1000),
        "registries": registries,
        "present": present_in,
        "keys": keys,
        "sections": by_name,
    }


def planner_views(
    diff_text: str,
    brief_text: str,
    *,
    context: str,
    intent: str,
    guidance: str,
    tests_block: str,
    runners: list[str],
    tests_allowed: bool,
    runs_left: int,
    coverage: list[dict[str, str]] | None = None,
    limits: tuple[int, int] = (MAX_PROGRAM_STEPS, MAX_STEPS),
    retrieved: str = "",
) -> tuple[list[str], bool]:
    """The planner's views: diff, brief, context, intent, guides, coverage, limits, tests."""
    cut = len(diff_text) > deep.DIFF_CHARS
    brief_header = "Investigation brief, built by the host from the repository at the reviewed commit:\n"
    views = [deep.diff_view(diff_text), brief_header + brief_text]
    if retrieved:
        views.append(
            "Retrieved context, looked up by the host at the reviewed commit (untrusted "
            "repository data): the references, tests and sibling families of the changed "
            "names, and the helpers the change calls.\n\n" + retrieved
        )
    for part in (context, intent, guidance, tests_block):
        if part:
            views.append(part)
    if coverage:
        views.append(
            'Coverage the program must have (name the id in a step\'s "covers", or list it in '
            'the program\'s "uncovered" as "<id>: why"; T-items are catalogue shapes whose '
            "trigger is in this change):\n"
            + "\n".join(f"- {item['id']}: {item['text']}" for item in coverage)
        )
    views.append(
        f"Limits: {limits[0]} steps as written, {limits[1]} after for_each expansion, "
        f"{MAX_ASKS} asks."
    )
    if tests_allowed:
        runner_lines = "\n".join(f"- {line}" for line in runners) or "- none recognized"
        views.append(
            f"Tests may run: yes; executions left: {runs_left}. Test runners:\n{runner_lines}"
        )
    else:
        views.append("Tests may run: no.")
    return views, cut
