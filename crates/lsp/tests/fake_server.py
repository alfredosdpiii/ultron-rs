#!/usr/bin/env python3
"""A language server for tests: every line holding ERROR is an error, every line holding WARN a warning.
FAKE_LSP picks how it reports: push (versioned publishes), unversioned (publishes without a version: an empty one
before the errors, and none when a clean file stays clean, like typescript-language-server), pull (textDocument/diagnostic), loading (says it never
finishes loading), slow (never reports), die (exits when a document opens)."""
import json
import os
import sys
import time

MODE = os.environ.get("FAKE_LSP", "push")
docs = {}
last = {}


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


def read():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.lower() == b"content-length":
            length = int(value)
    return json.loads(sys.stdin.buffer.read(length))


def diagnostics(text):
    found = []
    for number, line in enumerate(text.split("\n")):
        for word, severity in (("ERROR", 1), ("WARN", 2)):
            if word in line:
                at = {"line": number, "character": line.index(word)}
                found.append({"range": {"start": at, "end": at}, "severity": severity, "code": "E1",
                              "message": "%s found\n  on line %d" % (word, number + 1)})
    return found


def changed(uri, version, text):
    docs[uri] = text
    if MODE == "die":
        sys.exit(3)
    found = diagnostics(text)
    if MODE == "push":
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": uri, "version": version, "diagnostics": found}})
    elif MODE == "unversioned" and (found or last.get(uri) != []):
        if found:
            # The syntax pass finds nothing; the errors come with the semantic pass.
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": []}})
            time.sleep(0.1)
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": found}})
    last[uri] = found


print("fake server banner", flush=True)
while True:
    message = read()
    if message is None:
        break
    method, params = message.get("method"), message.get("params") or {}
    if method == "initialize":
        capabilities = {"diagnosticProvider": {"interFileDependencies": True}} if MODE in ("pull", "loading") else {}
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": capabilities}})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": "config-1", "method": "workspace/configuration", "params": {"items": [{"section": "fake"}]}})
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"health": "ok", "quiescent": MODE != "loading"}})
    elif method == "textDocument/didOpen":
        changed(params["textDocument"]["uri"], params["textDocument"]["version"], params["textDocument"]["text"])
    elif method == "textDocument/didChange":
        changed(params["textDocument"]["uri"], params["textDocument"]["version"], params["contentChanges"][0]["text"])
    elif method == "textDocument/diagnostic":
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": {"kind": "full", "items": diagnostics(docs.get(params["textDocument"]["uri"], ""))}})
