"""Persistent Socratic + Ultralearning state, calibration rail, and knowledge-graph visualizations."""
from __future__ import annotations

import json
import os
import re
import time
from pathlib import Path
from typing import Any

LEARNING_DIR_ENV = "ULTRON_LEARNING_DIR"
ACTIVE_STATE_FILE = "_active.json"
CONFIDENCE_STOPS = (0, 30, 50, 70, 90, 100)
CONFIDENCE_ANSI = (
    "\x1b[38;2;239;68;68m",   # 0   red
    "\x1b[38;2;249;115;22m",  # 30  orange
    "\x1b[38;2;234;179;8m",   # 50  amber
    "\x1b[38;2;132;204;22m",  # 70  lime
    "\x1b[38;2;34;197;94m",   # 90  green
    "\x1b[38;2;16;185;129m",  # 100 emerald
)
ANSI_RESET = "\x1b[0m"
ANSI_DIM = "\x1b[2m"

LEVEL_LABELS = {
    0: "unaware",
    1: "heard of it",
    2: "can follow",
    3: "with help",
    4: "independent",
    5: "can teach",
}

_SLUG_RE = re.compile(r"[^a-z0-9]+")


def learning_dir() -> Path:
    """Directory holding plain-JSON learning records (`~/.ultron/learning` or `ULTRON_LEARNING_DIR`)."""
    override = os.environ.get(LEARNING_DIR_ENV, "").strip()
    root = Path(override).expanduser() if override else Path.home() / ".ultron" / "learning"
    root.mkdir(parents=True, exist_ok=True)
    return root


def topic_slug(topic: str) -> str:
    """Filesystem-safe slug for a topic name."""
    cleaned = _SLUG_RE.sub("-", topic.strip().lower()).strip("-")
    if not cleaned:
        raise ValueError("topic must contain at least one alphanumeric character")
    return cleaned[:80]


def topic_path(topic: str) -> Path:
    return learning_dir() / f"{topic_slug(topic)}.json"


def _now_iso() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def _snap_confidence(value: int | float) -> int:
    num = max(0, min(100, int(round(value))))
    return min(CONFIDENCE_STOPS, key=lambda stop: abs(stop - num))


def _active_path() -> Path:
    return learning_dir() / ACTIVE_STATE_FILE


def active_topic() -> str | None:
    """Return the persisted active learning topic name, or None."""
    path = _active_path()
    if not path.is_file():
        return None
    data = json.loads(path.read_text(encoding="utf-8"))
    topic = data.get("topic") if isinstance(data, dict) else None
    return topic if isinstance(topic, str) and topic.strip() else None


def set_active(topic: str) -> None:
    """Persist `topic` as the active learning topic across sessions."""
    payload = {"topic": topic.strip(), "slug": topic_slug(topic), "updated_at": _now_iso()}
    path = _active_path()
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    tmp.replace(path)


def _resolve_topic(topic: str | None) -> str:
    if topic and topic.strip():
        return topic.strip()
    current = active_topic()
    if current:
        return current
    topics = list_topics()
    if topics:
        latest = max(topics, key=lambda item: item.get("updated_at", ""))
        return str(latest["topic"])
    raise FileNotFoundError("No active or saved learning topic found")


def _save(state: dict[str, Any]) -> Path:
    state["updated_at"] = _now_iso()
    path = topic_path(state["topic"])
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(state, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    tmp.replace(path)
    set_active(state["topic"])
    return path


def load(topic: str | None = None) -> dict[str, Any] | None:
    """Load a saved learning session by topic name/slug (or active topic if omitted), or None."""
    if topic is None or not topic.strip():
        current = active_topic()
        if not current:
            return None
        topic = current
    path = topic_path(topic)
    if not path.is_file():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def switch(topic: str) -> dict[str, Any]:
    """Switch the persisted active topic to an existing saved topic and return its state."""
    state = load(topic)
    if state is None:
        raise FileNotFoundError(f"No saved learning session for {topic!r}")
    set_active(state["topic"])
    return state


def list_topics() -> list[dict[str, Any]]:
    """Return summaries of all saved learning topics."""
    current = active_topic()
    items: list[dict[str, Any]] = []
    for path in sorted(learning_dir().glob("*.json")):
        if path.name == ACTIVE_STATE_FILE:
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict) or not data.get("topic"):
            continue
        items.append(
            {
                "topic": data["topic"],
                "slug": path.stem,
                "active": data["topic"] == current,
                "phase": data.get("phase", "teach"),
                "mastery_pct": overall_mastery_pct(data),
                "due_count": _due_count(data),
                "updated_at": data.get("updated_at", ""),
            }
        )
    return items


def _due_count(state: dict[str, Any]) -> int:
    turn = int(state.get("turn", 0))
    return sum(
        1
        for item in state.get("subskills", [])
        if item["level"] > 0
        and item["last_turn"] > 0
        and (turn - int(item["last_turn"])) >= int(item.get("interval_turns", 3))
    )


def start(
    topic: str,
    subskills: list[str],
    *,
    concepts: list[str] | tuple[str, ...] = (),
    facts: list[str] | tuple[str, ...] = (),
    procedures: list[str] | tuple[str, ...] = (),
    initial_levels: dict[str, int] | None = None,
    target_level: int = 4,
) -> dict[str, Any]:
    """Create (or reset) a persistent learning plan for `topic`."""
    if not subskills:
        raise ValueError("subskills must be non-empty")
    levels = {} if initial_levels is None else initial_levels
    clamped_target = max(1, min(5, int(target_level)))
    entries = []
    for name in subskills:
        lvl = max(0, min(5, int(levels.get(name, 0))))
        entries.append(
            {
                "name": name,
                "level": lvl,
                "checks_at_level": 0,
                "unprompted_at_level": 0,
                "streak": 0,
                "interval_turns": 3,
                "last_turn": 0,
                "misconceptions": [],
            }
        )
    state: dict[str, Any] = {
        "topic": topic.strip(),
        "phase": "teach",
        "target_level": clamped_target,
        "turn": 0,
        "focus": subskills[0],
        "metalearning": {
            "concepts": list(concepts),
            "facts": list(facts),
            "procedures": list(procedures),
        },
        "subskills": entries,
        "propositions": [],
        "calibration": [],
        "created_at": _now_iso(),
        "updated_at": _now_iso(),
    }
    _save(state)
    return state


def _find_subskill(state: dict[str, Any], name: str) -> dict[str, Any]:
    lowered = name.strip().lower()
    for item in state["subskills"]:
        if item["name"].strip().lower() == lowered:
            return item
    raise KeyError(f"Unknown subskill {name!r} in topic {state['topic']!r}")


def record_check(
    topic: str,
    subskill: str,
    *,
    correct: bool,
    confidence: int | float | None = None,
    unprompted: bool = False,
    misconception: str | None = None,
) -> dict[str, Any]:
    """Record one evaluation check, updating earned level, spaced interval, and calibration."""
    state = load(topic)
    if state is None:
        raise FileNotFoundError(f"No saved learning session for {topic!r}")
    item = _find_subskill(state, subskill)
    state["turn"] = int(state.get("turn", 0)) + 1
    turn = state["turn"]
    state["focus"] = item["name"]

    if correct:
        item["checks_at_level"] += 1
        if unprompted:
            item["unprompted_at_level"] += 1
        item["streak"] += 1
        item["interval_turns"] = min(20, max(3, int(item.get("interval_turns", 3)) * 2))
        if item["level"] < 5 and item["checks_at_level"] >= 2 and item["unprompted_at_level"] >= 1:
            item["level"] += 1
            item["checks_at_level"] = 0
            item["unprompted_at_level"] = 0
    else:
        item["streak"] = 0
        item["checks_at_level"] = 0
        item["unprompted_at_level"] = 0
        item["interval_turns"] = 2
        if item["level"] > 0:
            item["level"] -= 1
        if misconception:
            item["misconceptions"].append(misconception.strip())

    item["last_turn"] = turn
    if confidence is not None:
        snapped = _snap_confidence(confidence)
        state["calibration"].append(
            {
                "turn": turn,
                "subskill": item["name"],
                "confidence": snapped,
                "correct": bool(correct),
                "overconfident": bool(snapped >= 70 and not correct),
            }
        )

    _save(state)
    return state


def add_proposition(
    topic: str,
    prop_id: str,
    name: str,
    statement: str,
    *,
    subskill: str = "",
    premises: list[str] | tuple[str, ...] = (),
) -> dict[str, Any]:
    """Add or update a proposition and its premise links (`A ∧ B ⟹ C`) in the knowledge graph."""
    state = load(topic)
    if state is None:
        raise FileNotFoundError(f"No saved learning session for {topic!r}")
    entry = {
        "id": prop_id.strip(),
        "name": name.strip(),
        "statement": statement.strip(),
        "subskill": subskill.strip(),
        "premises": [p.strip() for p in premises if p.strip()],
        "created_at": _now_iso(),
    }
    props: list[dict[str, Any]] = state.setdefault("propositions", [])
    for idx, existing in enumerate(props):
        if existing["id"] == entry["id"]:
            props[idx] = entry
            _save(state)
            return state
    props.append(entry)
    _save(state)
    return state


def due_reviews(topic: str | None = None) -> list[dict[str, Any]]:
    """Return subskills due for interleaved spaced retrieval."""
    state = load(topic)
    if state is None:
        return []
    turn = int(state.get("turn", 0))
    due: list[dict[str, Any]] = []
    for item in state["subskills"]:
        if item["level"] == 0 or item["last_turn"] == 0:
            continue
        elapsed = turn - int(item["last_turn"])
        if elapsed >= int(item.get("interval_turns", 3)):
            due.append(item)
    return due


def bottleneck(topic: str | None = None) -> dict[str, Any] | None:
    """Return the lowest-mastery subskill below target level (for Ultralearning Drill)."""
    state = load(topic)
    if state is None or not state["subskills"]:
        return None
    target = int(state.get("target_level", 4))
    below = [item for item in state["subskills"] if item["level"] < target]
    pool = below if below else state["subskills"]
    return min(pool, key=lambda item: (item["level"], -len(item.get("misconceptions", []))))


def overall_mastery_pct(state: dict[str, Any]) -> int:
    items = state.get("subskills", [])
    if not items:
        return 0
    target = max(1, int(state.get("target_level", 4)))
    total = sum(min(item["level"], target) for item in items)
    return int(round((total / (len(items) * target)) * 100))


def _bar(filled: int, width: int = 10) -> str:
    clamped = max(0, min(width, filled))
    return "█" * clamped + "░" * (width - clamped)


def render_skill_map(state: dict[str, Any]) -> str:
    """Render Jane Street's 0-5 Unicode skill map with mastery %, focus, and due-review markers."""
    pct = overall_mastery_pct(state)
    overall_bar = _bar(int(round(pct / 10)), 10)
    target = int(state.get("target_level", 4))
    turn = int(state.get("turn", 0))
    focus = state.get("focus", "")
    width = max((len(item["name"]) for item in state["subskills"]), default=12)
    width = min(max(width, 16), 34)

    lines = [
        f"Topic: {state['topic']}   overall: {overall_bar} {pct}% (target {target}/5)",
        "─" * 68,
    ]
    for item in state["subskills"]:
        lvl = int(item["level"])
        bar = _bar(lvl * 2, 10)
        tag = ""
        if lvl >= target:
            tag = f"  ★ {LEVEL_LABELS.get(lvl, 'mastered')}"
        elif item["name"] == focus:
            tag = "  ← current focus"
        elif item["last_turn"] > 0 and (turn - item["last_turn"]) >= item.get("interval_turns", 3):
            tag = "  ↻ review due"
        label = item["name"][:width].ljust(width)
        lines.append(f"{label}  [{bar}]  {lvl}/5{tag}")
    lines.append("─" * 68)
    return "\n".join(lines)


def render_confidence_rail(confidence: int | float, *, ansi: bool = True) -> str:
    """Render LearnHarness's 6-stop confidence rail (0 red -> 100 emerald)."""
    snapped = _snap_confidence(confidence)
    idx = CONFIDENCE_STOPS.index(snapped)
    filled = int(round(snapped / 10))
    if not ansi:
        return f"[{_bar(filled, 10)}] {snapped}%"
    color = CONFIDENCE_ANSI[idx]
    solid = f"{color}{'█' * filled}{ANSI_RESET}"
    empty = f"{ANSI_DIM}{'░' * (10 - filled)}{ANSI_RESET}"
    return f"[{solid}{empty}] {color}{snapped}%{ANSI_RESET}"


def render_calibration(state: dict[str, Any], *, ansi: bool = True) -> str:
    """Summarize predicted confidence vs actual accuracy and highlight overconfidence traps."""
    records = state.get("calibration", [])
    if not records:
        return "Calibration: no confidence-rated checks yet (rate 0/30/50/70/90/100%)."
    avg_conf = sum(r["confidence"] for r in records) / len(records)
    accuracy = (sum(1 for r in records if r["correct"]) / len(records)) * 100.0
    traps = [r for r in records if r.get("overconfident")]
    rail = render_confidence_rail(avg_conf, ansi=ansi)
    gap = avg_conf - accuracy
    bias = "well-calibrated" if abs(gap) <= 10 else ("overconfident" if gap > 10 else "underconfident")
    lines = [
        f"Confidence rail: {rail}  actual accuracy: {accuracy:.0f}% ({bias}, n={len(records)})",
    ]
    if traps:
        trap_names = ", ".join(dict.fromkeys(t["subskill"] for t in traps[-4:]))
        lines.append(f"Overconfidence traps (≥70% confident & wrong): {len(traps)} [{trap_names}]")
    return "\n".join(lines)


def render_chain_table(state: dict[str, Any]) -> str:
    """Render LearnHarness's reasoning-chain table using real logic connectives (`A ∧ B ⟹ C`)."""
    props = state.get("propositions", [])
    if not props:
        return "Reasoning chains: none recorded yet (derived propositions accumulate as `A ∧ B ⟹ C`)."
    by_id = {p["id"]: p["name"] for p in props}
    lines = ["Reasoning chains (premises ∧ … ⟹ conclusion):"]
    for prop in props:
        premises = [by_id.get(pid, pid) for pid in prop.get("premises", [])]
        if premises:
            chain = f"  {' ∧ '.join(premises)} ⟹ {prop['name']}"
        else:
            chain = f"  {prop['name']}  (base proposition)"
        lines.append(chain)
        lines.append(f"      {prop['statement']}")
    return "\n".join(lines)


def mermaid_graph(state: dict[str, Any]) -> str:
    """Generate a Mermaid flowchart for `await diagram.mermaid(...)` showing subskills & chains."""
    props = state.get("propositions", [])
    lines = ["flowchart LR"]
    if props:
        for prop in props:
            nid = re.sub(r"[^A-Za-z0-9_]", "_", prop["id"])
            label = prop["name"].replace('"', "'")
            lines.append(f'  {nid}["{label}"]')
        for prop in props:
            target = re.sub(r"[^A-Za-z0-9_]", "_", prop["id"])
            for pid in prop.get("premises", []):
                src = re.sub(r"[^A-Za-z0-9_]", "_", pid)
                lines.append(f"  {src} --> {target}")
        return "\n".join(lines)

    for idx, item in enumerate(state.get("subskills", [])):
        nid = f"s{idx}"
        label = f"{item['name']} ({item['level']}/5)".replace('"', "'")
        lines.append(f'  {nid}["{label}"]')
        if idx > 0:
            lines.append(f"  s{idx - 1} --> {nid}")
    return "\n".join(lines)


def render_dashboard(topic: str | None = None, *, ansi: bool = True) -> str:
    """Render the full terminal learning dashboard (skill map, calibration rail, chains)."""
    resolved = _resolve_topic(topic)
    state = load(resolved)
    if state is None:
        return f"No saved learning session for {resolved!r}."
    parts = [
        render_skill_map(state),
        render_calibration(state, ansi=ansi),
        render_chain_table(state),
    ]
    return "\n\n".join(parts)
