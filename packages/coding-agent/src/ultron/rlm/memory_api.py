class Memory:
    def __init__(self, bridge): self._bridge = bridge
    # Calls made on purpose (the Rust host also recalls and keeps on its own when automatic memory is on). A write holding a secret is refused.
    async def prepare(self, query, *, scope='session', task_id='rlm'):
        return await self._bridge.request('memory.prepare', {'query': query, 'scope': scope, 'taskId': task_id})
    async def why(self, task_id): return await self._bridge.request('memory.why', {'taskId': task_id})
    async def propose(self, text, evidence, *, scope='session', type=None, key=None, situation=None, path_globs=None):
        return await self._bridge.request('memory.propose', _propose(text, evidence, scope, type, key, situation, path_globs))
    async def correct(self, memory_id, text, evidence): return await self._bridge.request('memory.correct', {'id': memory_id, 'text': text, 'evidence': evidence})
    async def forget(self, memory_id): return await self._bridge.request('memory.forget', {'id': memory_id})
    async def get(self, memory_id): return await self._bridge.request('memory.get', {'id': memory_id})
    async def list(self): return await self._bridge.request('memory.list')
    # reflect answers a question from memory; invalidate retires a wrong [fact <id>] a recall showed;
    # for an observation it retires nothing and returns its source facts: invalidate the wrong ones.
    async def reflect(self, query, *, scope=None): return await self._bridge.request('memory.reflect', {'query': query} if scope is None else {'query': query, 'scope': scope})
    async def invalidate(self, fact_id, reason=None): return await self._bridge.request('memory.invalidate', {'id': fact_id} if reason is None else {'id': fact_id, 'reason': reason})
    async def dream(self, *, days=7, limit=50, project=None):
        """Re-check kept claims not confirmed for `days` (memorantado only). Quotes that still hold confirm a claim
        with no model call; a changed quote is judged (judge.decide): supported is revised, stale or contradicted
        retracted, unsure left in 'review'. A recent claim that shares its situation with an older one is checked
        for a contradiction and flagged as a conflict. Returns {confirmed, revised, retracted, conflicts, review,
        unchecked, judged}. `project` (a directory) checks that project's claims against its files instead of
        this session's."""
        payload = {'days': days, 'limit': limit}
        if project is not None: payload['project'] = project
        return await self._bridge.request('memory.dream', payload)
    async def brief(self, text, *, scope='project', topic=None, project=None):
        """Write the startup brief of `scope` ('project', or 'global' for the user's brief), at most 200 lines and
        4000 characters, or with `topic` (kebab-case) a topic page (120 lines, 6000 characters). It replaces the one
        before and opens each new context (memorantado only: Hindsight writes its own). `project` (a directory)
        names another project than this session's."""
        payload = {'text': text, 'scope': scope}
        if topic is not None: payload['topic'] = topic
        if project is not None: payload['project'] = project
        return await self._bridge.request('memory.brief', payload)
    async def topic(self, name=None, *, scope='project', project=None):
        """The topic page `name` of `scope` ({topic, content, refreshedAt}, None when there is none); without a
        name, the names of the scope's topic pages."""
        payload = {'scope': scope}
        if name is not None: payload['topic'] = name
        if project is not None: payload['project'] = project
        return await self._bridge.request('memory.topic', payload)

class Refinements:
    def __init__(self, bridge): self._bridge = bridge
    async def propose(self, kind, target, base_version, content, evidence, scope='session'):
        return await self._bridge.request('refinements.propose', {'kind':kind,'target':target,'baseVersion':base_version,'content':content,'evidence':evidence,'scope':scope})
    async def list(self): return await self._bridge.request('refinements.list')
    async def activate(self, id): return await self._bridge.request('refinements.activate', {'id':id})
    async def rollback(self, id): return await self._bridge.request('refinements.rollback', {'id':id})
    async def reject(self, id): return await self._bridge.request('refinements.reject', {'id':id})


def _propose(text, evidence, scope, type, key, situation, path_globs):
    # Typed records (sent only when given): type is preference, fact, procedure, pitfall or
    # reference; situation says in one line when it applies; key (kebab-case) replaces the record with that key;
    # path_globs (e.g. ['src/**/*.rs']) make a matching file change point at the record. An evidence item may quote
    # the file its ref names ({'ref': 'src/a.rs:12', 'quote': '<text copied from it, at most 300 bytes>'}): a later
    # recall marks the memory stale once the file no longer holds that text.
    payload = {'text': text, 'evidence': evidence, 'scope': scope}
    for name, value in (('type', type), ('key', key), ('situation', situation), ('pathGlobs', path_globs)):
        if value is not None:
            payload[name] = list(value) if name == 'pathGlobs' else value
    return payload
