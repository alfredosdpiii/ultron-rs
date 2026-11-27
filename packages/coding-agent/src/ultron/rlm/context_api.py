"""Model-owned context control (`ctx`): the model edits what it sees, never the durable transcript.

Every call acts on the calling lane's current conversation branch. Edits are Pi-style context edits:
they change only the model's view on this branch, and the transcript keeps every item.
"""

import hashlib

from infer_api import ContextHandle


class Context:
    """`ctx` manages your own context; the durable transcript itself is never changed.

    - `await ctx.history(limit=20, kinds=None, cache_impact=False)` lists recent items with id, kind, bytes,
      preview and state (pass `cache_impact=True` to include `cache_impact_bytes` for prefix-cache cost).
    - `await ctx.export(kinds=None)` returns the active visible branch as a `ContextHandle` (`h.search(...)`,
      `h.lines(...)`, `h.chunks(...)`) so you can inspect your own context in Python.
    - `await ctx.get(id)` returns one item in full; `await ctx.forget(ids, reason)` removes items from what
      you see; `await ctx.summarize(ids, text)` replaces a span with your summary; `await ctx.pin(id)` keeps
      an item through edits and compaction; `await ctx.note(text)` records a note that survives compaction.
    - The current user message and pinned items cannot be forgotten.
    - Task results collapse on return: after you have seen a cell's output once, it shrinks to one line per
      finished task (definition, key, status, cost). `await agents.result("<task id>")` returns the full
      value again; `await ctx.get(id)` shows the original output.
    """

    def __init__(self, bridge):
        self._bridge = bridge

    async def history(self, limit=20, kinds=None, cache_impact=False):
        """Most recent items in the model's context (oldest first), each with id, kind, bytes, preview and state.

        kinds: optional list of "user", "assistant", "tool", "custom", "note", "summary", "compaction".
        cache_impact: when True, include `cache_impact_bytes` (visible bytes after each item in the window).
        """
        payload = {'limit': limit}
        if kinds is not None:
            payload['kinds'] = [kinds] if isinstance(kinds, str) else list(kinds)
        if cache_impact:
            payload['cache_impact'] = True
        return await self._bridge.request('ctx.history', payload)

    async def export(self, kinds=None):
        """Return the active visible context window as a ContextHandle for regex search and slicing."""
        payload = {}
        if kinds is not None:
            payload['kinds'] = [kinds] if isinstance(kinds, str) else list(kinds)
        result = await self._bridge.request('ctx.export', payload)
        text = result.get('text', '') if isinstance(result, dict) else ''
        raw = text.encode('utf-8')
        digest = hashlib.sha256(raw).hexdigest()
        return ContextHandle(text, digest, 'ctx', len(raw))

    async def get(self, id):
        """One item in full: the durable message and what the model currently sees of it."""
        return await self._bridge.request('ctx.get', {'id': id})

    async def forget(self, ids, reason):
        """Remove items from the model's context. The current user message and pinned items are refused."""
        return await self._bridge.request('ctx.forget', {'ids': [ids] if isinstance(ids, str) else list(ids), 'reason': reason})

    async def summarize(self, ids, text):
        """Replace a span of items with your own summary text (tool calls and their results move together)."""
        return await self._bridge.request('ctx.summarize', {'ids': [ids] if isinstance(ids, str) else list(ids), 'text': text})

    async def pin(self, id):
        """Protect an item from forget, summarize, collapse, and compaction."""
        return await self._bridge.request('ctx.pin', {'id': id})

    async def unpin(self, id):
        return await self._bridge.request('ctx.unpin', {'id': id})

    async def note(self, text):
        """Append a durable note to your context; it stays visible after compaction."""
        return await self._bridge.request('ctx.note', {'text': text})

    async def prune(self, target_bytes=None, *, kinds=None, dry_run=False):
        """Cache-aware prune: forget editable items ranked by bytes / (cache_impact_bytes + 1).

        Defaults to pruning ``["tool"]`` outputs; pass ``dry_run=True`` to inspect candidates first.
        """
        payload = {}
        if target_bytes is not None:
            payload['target_bytes'] = target_bytes
        if kinds is not None:
            payload['kinds'] = [kinds] if isinstance(kinds, str) else list(kinds)
        if dry_run:
            payload['dry_run'] = True
        return await self._bridge.request('ctx.prune', payload)
