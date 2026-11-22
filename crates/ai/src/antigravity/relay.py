#!/usr/bin/env python3
"""Ultron's agy MCP relay: execs ULTRON_AGY_MCP_EXEC (`ultron agy`), relays ULTRON_AGY_BRIDGE_URL (`agy acp`), or lists no tools."""
import json, os, sys, threading, urllib.request

EXEC = os.environ.get("ULTRON_AGY_MCP_EXEC")
if EXEC:
    argv = json.loads(EXEC)
    os.execvp(argv[0], argv)

URL = os.environ.get("ULTRON_AGY_BRIDGE_URL")
TOKEN = os.environ.get("ULTRON_AGY_BRIDGE_TOKEN", "")
lock = threading.Lock()


def out(message):
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def local(message):
    if "id" not in message:
        return
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": message["params"].get("protocolVersion"), "capabilities": {"tools": {}},
                  "serverInfo": {"name": "ultron", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": []}
    else:
        result = {}
    out({"jsonrpc": "2.0", "id": message["id"], "result": result})


def relay(url, line):
    request = urllib.request.Request(url, line.encode(), {"content-type": "application/json", "authorization": f"Bearer {TOKEN}"})
    try:
        with urllib.request.urlopen(request, timeout=3600) as response:
            body = response.read()
        if body:
            out(json.loads(body))
    except Exception as error:
        message = json.loads(line)
        if "id" in message:
            out({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": str(error)}})


for line in sys.stdin:
    if not line.strip():
        continue
    if URL:
        threading.Thread(target=relay, args=(URL, line), daemon=True).start()
    else:
        local(json.loads(line))
