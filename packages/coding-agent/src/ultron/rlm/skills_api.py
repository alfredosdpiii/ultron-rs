"""Skill catalog client. Selection is explainable and version-pinned; skills never grant capabilities."""


class Skills:
    """Skill catalog and code skills.

    Code skills are tested Python kept across sessions. When a procedure worked and will recur, save it:
    `await skills.propose_code(name, source, test_source, evidence)` (name is a lowercase identifier;
    `test_source` defines `test_*` functions and imports the skill with `from code_skills import <name>`).
    The test runs in a fresh kernel; only a passing version becomes active. `await skills.rollback(name,
    version)` undoes the active version; `await skills.code_list()` and `await skills.code_history(name)`
    inspect them. Use an active code skill instead of re-deriving it: `from code_skills import <name>` works
    in every kernel.

    Procedure skills (a `procedure:` in their SKILL.md frontmatter) are walked, not read:
    `run = await skills.run(name, input={...})` starts one (see `help(SkillRun)`); the host checks every
    segment itself. `await skills.coverage(name)` summarizes this session's runs of a skill.
    """

    def __init__(self, bridge, namespace=None):
        self._bridge = bridge
        self._namespace = namespace if namespace is not None else {}

    async def refresh(self):
        return await self._bridge.request('skills.refresh', {})

    async def list(self):
        return await self._bridge.request('skills.list', {})

    async def select(self, query, *, limit=None):
        payload = {'query': query}
        if limit is not None:
            payload['limit'] = limit
        return await self._bridge.request('skills.select', payload)

    async def load(self, name, *, version=None):
        payload = {'name': name}
        if version is not None:
            payload['version'] = version
        return await self._bridge.request('skills.load', payload)

    async def why(self, decision_id):
        return await self._bridge.request('skills.why', {'decision_id': decision_id})

    async def invoke(self, name, version, input):
        return await self._bridge.request('skills.invoke', {'name': name, 'version': version, 'input': input})

    async def run(self, name, input=None, *, version=None):
        """Start skill `name`'s procedure with `input` (the procedure's declared inputs); returns a SkillRun at its
        first segment (its setup already run). Raises if the skill has no procedure, an input is missing, or this
        lane already runs one. A run whose first segment cannot begin comes back with outcome "blocked"."""
        payload = {'name': name}
        if input is not None:
            payload['input'] = input
        if version is not None:
            payload['version'] = version
        run = SkillRun(self._bridge, self._namespace, await self._bridge.request('procedure.start', payload))
        await run._setup()
        return run

    async def coverage(self, name):
        """This session's runs of skill `name` at its current version: {"runs", "outcomes": {outcome: n},
        "segments": [{name, kind, reached, passed, failed, mean_tries}], "never_passed", "other_versions"}."""
        return await self._bridge.request('procedure.coverage', {'name': name})

    async def propose_procedure(self, name, content, evidence):
        """Propose a procedure skill for the user to publish: `content` is the whole SKILL.md (frontmatter `name`,
        `description` and a `procedure:` that parses); `evidence` is [{"ref": "session://<id>#<from>..<to>" or
        "skill-run:<id>", "quote": "..."}] showing where the procedure worked. It is kept as a proposal only;
        the user publishes it with `ultron skills approve <id>` (or rejects it)."""
        return await self._bridge.request('procedure.propose', {'name': name, 'content': content, 'evidence': evidence})

    # Code skills (procedural memory as tested code) import as `from code_skills import <name>`. The package is
    # named `code_skills` so it never shadows this `skills` object.

    async def propose_code(self, name, source, test_source, evidence):
        """Write `name` as a new version, run its test in a fresh kernel, and activate it only on pass."""
        return await self._bridge.request('skills.propose_code', {
            'name': name,
            'source': _source_text(source, 'source'),
            'test_source': _source_text(test_source, 'test_source'),
            'evidence': evidence,
        })

    async def propose_policy(self, name, plan, test_source, evidence, *, family=None):
        """Store a decomposition plan (a module defining `plan(...)`, or a plan function) as a tested policy skill."""
        payload = {
            'name': name,
            'source': _plan_source(plan),
            'test_source': _source_text(test_source, 'test_source'),
            'evidence': evidence,
        }
        if family is not None:
            payload['family'] = family
        return await self._bridge.request('skills.propose_policy', payload)

    async def rollback(self, name, version):
        """Roll back the active version `version` of a code skill; the version it replaced becomes active again."""
        return await self._bridge.request('skills.rollback', {'name': name, 'version': version})

    async def code_list(self, *, kind=None, family=None):
        """Active code and policy skills with their docstrings and versions."""
        payload = {}
        if kind is not None:
            payload['kind'] = kind
        if family is not None:
            payload['family'] = family
        return await self._bridge.request('skills.code_list', payload)

    async def code_history(self, name):
        """Every version of a code skill with its state, test outcome, and secret check."""
        return await self._bridge.request('skills.code_history', {'name': name})


class SkillRun:
    """A procedure skill's run, walked one segment at a time; the host decides every step.

    `run.segment` is the segment to do now: {"name", "goal", "kind", "writes", "max_iters", "check" |
    "questions"/"pass" | "model"/"checks", "requires"?, "setup"}. Do its goal in your own cells, changing only
    files matching its `writes` globs (a cell that changes others is told so, and the segment's next step
    fails), then `await run.check()`:

    - kind "check": the host runs the segment's command (`expect` "fail" means the command must fail);
    - kind "judge": `await run.check(state="what you claim and why")`; the judge reads it beside the last
      check's output and must answer as `pass` says;
    - kind "agent": the host runs a subagent on the segment's model with the goal and checks its work.

    The reply (also `run.status`) has "outcome": "running" (with the next or same "segment", "tries_left"
    and "step": {"passed", "evidence"}), or the end: "passed", "failed" (a segment used all its tries),
    "blocked" (a segment's requires did not hold), "needs_review" (the judge stayed unsure: ask the user),
    "final_check_mismatch" (the host ran the final check again at the end and it no longer passed) or
    "aborted" (`await run.abort(reason)`). A new segment's `setup` code runs in your namespace when it begins.
    """

    def __init__(self, bridge, namespace, status):
        self._bridge = bridge
        self._namespace = namespace
        self.status = status

    @property
    def id(self):
        return self.status['run']

    @property
    def outcome(self):
        return self.status['outcome']

    @property
    def segment(self):
        return self.status.get('segment')

    async def check(self, state=None):
        """Ask the host to check the current segment; returns the new status."""
        if self.outcome != 'running':
            raise RuntimeError(f'skill run {self.id} has ended: {self.outcome}')
        before = self.segment['name']
        payload = {'run': self.id, 'segment': before}
        if state is not None:
            payload['state'] = state
        self.status = await self._bridge.request('procedure.step', payload)
        if self.outcome == 'running' and self.segment['name'] != before:
            await self._setup()
        return self.status

    async def abort(self, reason=None):
        """End the run as "aborted"; returns its final status."""
        payload = {'run': self.id}
        if reason is not None:
            payload['reason'] = reason
        self.status = await self._bridge.request('procedure.abort', payload)
        return self.status

    async def _setup(self):
        code = (self.segment or {}).get('setup') if self.outcome == 'running' else None
        if not code:
            return
        import ast
        compiled = compile(code, f'<skill {self.status["name"]}: {self.segment["name"]} setup>', 'exec',
                           flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
        result = eval(compiled, self._namespace)
        if hasattr(result, '__await__'):
            await result

    def __repr__(self):
        segment = self.segment['name'] if self.segment else None
        return f'<SkillRun {self.status["name"]} {self.outcome} segment={segment}>'


def _source_text(value, label):
    if callable(value) and not isinstance(value, str):
        import inspect
        import textwrap
        try:
            return textwrap.dedent(inspect.getsource(value))
        except (OSError, TypeError) as error:
            raise ValueError(f'{label}: pass the source text; this callable has no retrievable source') from error
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f'{label} must be nonempty Python source text')
    return value


def _plan_source(plan):
    """A policy module must define `plan`; a plan function under another name is aliased to it."""
    source = _source_text(plan, 'plan')
    if callable(plan) and not isinstance(plan, str) and getattr(plan, '__name__', 'plan') != 'plan':
        source = source.rstrip() + f'\n\nplan = {plan.__name__}\n'
    return source


# `code_skills` package: active code skills, importable in every kernel.

import importlib.abc as _abc
import importlib.util as _util
import hashlib as _hashlib
import json as _json
import os as _os
import re as _re
import sys as _sys
import types as _types

CODE_SKILLS_PACKAGE = 'code_skills'
_NAME = _re.compile(r'^[a-z][a-z0-9_]{0,63}$')


def _skills_dir():
    value = _os.environ.get('ULTRON_CODE_SKILLS_DIR', '').strip()
    return value or None


def _active_entry(name):
    """(version, path) of the active version, verified against its recorded digest, or None."""
    root = _skills_dir()
    if root is None or not _NAME.match(name):
        return None
    base = _os.path.join(root, name)
    try:
        with open(_os.path.join(base, '.history.json'), encoding='utf-8') as handle:
            history = _json.load(handle)
    except (OSError, ValueError):
        return None
    active = history.get('active') if isinstance(history, dict) else None
    if not isinstance(active, int):
        return None
    record = next((item for item in history.get('versions', []) if isinstance(item, dict) and item.get('version') == active), None)
    if record is None or record.get('state') != 'active':
        return None
    path = _os.path.join(base, '.versions', str(active), 'skill.py')
    try:
        with open(path, 'rb') as handle:
            digest = _hashlib.sha256(handle.read()).hexdigest()
    except OSError:
        return None
    # Only the exact source whose test passed is importable.
    if digest != (record.get('sha256') or {}).get('skill'):
        return None
    return active, path


def _load(fullname, name, version, path):
    spec = _util.spec_from_file_location(fullname, path)
    module = _util.module_from_spec(spec)
    module.__skill_version__ = version
    module.__skill_name__ = name
    previous = _sys.modules.get(fullname)
    _sys.modules[fullname] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        if previous is None:
            _sys.modules.pop(fullname, None)
        else:
            _sys.modules[fullname] = previous
        raise
    return module


class _CodeSkillsPackage(_types.ModuleType):
    """`from code_skills import name` always resolves to the active version at the time of import."""

    def __init__(self):
        super().__init__(CODE_SKILLS_PACKAGE, 'Tested Python code skills. `from code_skills import <name>`.')
        object.__setattr__(self, '__path__', [])
        object.__setattr__(self, '_candidates', {})

    def __setattr__(self, key, value):
        # The import system sets loaded submodules as attributes; keep resolution fresh instead.
        if _NAME.match(key):
            return
        object.__setattr__(self, key, value)

    def _use_candidate(self, name, path, version=None):
        """Test kernels only: resolve `name` to an unactivated candidate file."""
        self._candidates[name] = (version, path)

    def _resolve(self, name):
        if name in self._candidates:
            return self._candidates[name]
        return _active_entry(name)

    def __getattr__(self, name):
        if name.startswith('__') or not _NAME.match(name):
            raise AttributeError(name)
        entry = self._resolve(name)
        if entry is None:
            # Drop a module cached from an earlier active version, which `from ... import` would fall back to.
            _sys.modules.pop(f'{CODE_SKILLS_PACKAGE}.{name}', None)
            raise AttributeError(f'no active code skill named {name!r} (see `await skills.code_list()`)')
        version, path = entry
        fullname = f'{CODE_SKILLS_PACKAGE}.{name}'
        cached = _sys.modules.get(fullname)
        if cached is not None and getattr(cached, '__skill_version__', None) == version and getattr(cached, '__file__', None) == path:
            return cached
        return _load(fullname, name, version, path)

    def __dir__(self):
        root = _skills_dir()
        names = set(self._candidates)
        if root is not None:
            try:
                names.update(entry for entry in _os.listdir(root) if _NAME.match(entry) and _active_entry(entry))
            except OSError:
                pass
        return sorted(names)


class _CodeSkillsFinder(_abc.MetaPathFinder):
    """Makes `import code_skills.name` and `from code_skills.name import f` work too."""

    def find_spec(self, fullname, path=None, target=None):
        if fullname == CODE_SKILLS_PACKAGE or not fullname.startswith(CODE_SKILLS_PACKAGE + '.'):
            return None
        name = fullname[len(CODE_SKILLS_PACKAGE) + 1:]
        package = _sys.modules.get(CODE_SKILLS_PACKAGE)
        if not isinstance(package, _CodeSkillsPackage) or '.' in name or not _NAME.match(name):
            return None
        entry = package._resolve(name)
        if entry is None:
            return None
        version, file_path = entry
        spec = _util.spec_from_file_location(fullname, file_path)
        loader = spec.loader
        original = loader.exec_module

        def exec_module(module):
            module.__skill_version__ = version
            module.__skill_name__ = name
            original(module)

        loader.exec_module = exec_module
        return spec


def install_code_skills():
    if not isinstance(_sys.modules.get(CODE_SKILLS_PACKAGE), _CodeSkillsPackage):
        _sys.modules[CODE_SKILLS_PACKAGE] = _CodeSkillsPackage()
    if not any(isinstance(finder, _CodeSkillsFinder) for finder in _sys.meta_path):
        _sys.meta_path.append(_CodeSkillsFinder())


# Every kernel imports this module at startup, so every kernel can import active code skills.
install_code_skills()
