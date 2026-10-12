#!/usr/bin/env python3
"""Execute queued shell requests inside the isolated, non-privileged sandbox container.

This worker runs only as UID 10001 in a Docker container with its own workspace.
It has no host filesystems, Docker socket, LHR database, or host credentials mounted.
"""
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import time

QUEUE = Path("/control/requests")
RESULTS = Path("/control/responses")
VALID_ID = re.compile(r"[0-9a-f]{32}\Z")
MAX_OUTPUT = 96 * 1024
MAX_COMMAND = 16 * 1024
MAX_TIME = 3600


def save_response(key, result):
    name = RESULTS / (key + ".json")
    temporary = RESULTS / ("." + key + ".tmp")
    with temporary.open("w", encoding="utf-8") as dest:
        json.dump({"request_id": key, **result}, dest)
        dest.flush()
        os.fsync(dest.fileno())
    temporary.replace(name)


def execute(request):
    script = request["command"]
    timeout = request["timeout_seconds"]
    cwd = request.get("cwd") or "/workspace"
    if not isinstance(script, str) or not script.strip() or len(script.encode()) > MAX_COMMAND:
        raise ValueError("invalid command")
    if type(timeout) is not int or not 1 <= timeout <= MAX_TIME:
        raise ValueError("invalid timeout")
    if not isinstance(cwd, str) or not cwd.startswith("/") or "\0" in cwd:
        raise ValueError("working directory must be absolute inside the sandbox")
    start = time.monotonic()
    process = subprocess.Popen(
        ["/bin/bash", "-lc", script],
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        env={**os.environ, "HOME": "/workspace"},
    )
    collected = bytearray()
    truncated = False
    timed_out = False
    assert process.stdout is not None
    descriptor = process.stdout.fileno()
    pipe_open = True
    deadline = start + timeout
    try:
        while pipe_open or process.poll() is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                break
            ready, _, _ = select.select(
                [descriptor] if pipe_open else [], [], [], min(remaining, 0.2)
            )
            if ready:
                block = os.read(descriptor, 65536)
                if not block:
                    pipe_open = False
                    continue
                space = max(0, MAX_OUTPUT - len(collected))
                collected.extend(block[:space])
                if len(block) > space:
                    truncated = True
        process.wait(timeout=5)
    finally:
        process.stdout.close()
        if process.poll() is None:
            process.kill()
            process.wait()
    return {
        "status": "timed_out" if timed_out else "complete",
        "exit_code": process.returncode,
        "stdout": collected.decode("utf-8", errors="replace"),
        "truncated": truncated,
        "elapsed_ms": int((time.monotonic() - start) * 1000),
    }


def process_pending():
    for request_path in sorted(QUEUE.glob("*.json")):
        key = request_path.stem
        if not VALID_ID.fullmatch(key) or request_path.is_symlink():
            continue
        response_path = RESULTS / request_path.name
        if response_path.exists():
            try:
                prior = json.loads(response_path.read_text(encoding="utf-8"))
                if prior.get("status") in {"running", "complete", "timed_out", "failed"}:
                    request_path.unlink(missing_ok=True)
                    continue
            except (OSError, ValueError):
                continue
        try:
            if request_path.stat().st_size > 64 * 1024:
                raise ValueError("request exceeds size limit")
            request = json.loads(request_path.read_text(encoding="utf-8"))
            if request.get("request_id") != key:
                raise ValueError("request ID mismatch")
            if set(request) != {"request_id", "command", "cwd", "timeout_seconds"}:
                raise ValueError("invalid request fields")
            save_response(key, {"status": "running"})
            result = execute(request)
        except Exception as exc:
            result = {"status": "failed", "error": str(exc)}
        save_response(key, result)
        request_path.unlink(missing_ok=True)


if __name__ == "__main__":
    QUEUE.mkdir(parents=True, exist_ok=True)
    RESULTS.mkdir(parents=True, exist_ok=True)
    while True:
        process_pending()
        time.sleep(0.3)
