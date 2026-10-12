"""Regression tests for the isolated Bash worker.

These use a temporary working directory on the CI runner; no host privileges,
Docker socket, network connection, or LHR dataset is needed.
"""
import importlib.util
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "sandbox_worker.py"
SPEC = importlib.util.spec_from_file_location("sandbox_worker", SCRIPT)
assert SPEC and SPEC.loader
worker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(worker)


def request(command: str, cwd: Path, timeout: int = 5) -> dict:
    return {
        "request_id": "1234567890abcdef1234567890abcdef",
        "command": command,
        "cwd": str(cwd),
        "timeout_seconds": timeout,
    }


def test_executes_real_bash_and_persists_files(tmp_path):
    first = worker.execute(request("printf 'hello' > marker.txt; echo first; pwd", tmp_path))
    assert first["status"] == "complete"
    assert first["exit_code"] == 0
    assert str(tmp_path) in first["stdout"]
    second = worker.execute(request("cat marker.txt", tmp_path))
    assert second["status"] == "complete"
    assert second["stdout"] == "hello"


def test_nonzero_exit_code_is_reported(tmp_path):
    result = worker.execute(request("printf 'bad' >&2; exit 9", tmp_path))
    assert result["status"] == "complete"
    assert result["exit_code"] == 9
    assert "bad" in result["stdout"]


def test_output_is_bounded(tmp_path):
    result = worker.execute(request("python3 -c 'print(\"X\" * 150000)'", tmp_path))
    assert result["status"] == "complete"
    assert result["truncated"]
    assert len(result["stdout"].encode("utf-8")) <= worker.MAX_OUTPUT


def test_timeout_stops_process_group(tmp_path):
    result = worker.execute(request("sleep 30", tmp_path, 1))
    assert result["status"] == "timed_out"


@pytest.mark.parametrize(
    "overrides",
    [
        {"command": ""},
        {"command": "x" * (16 * 1024 + 1)},
        {"timeout_seconds": 0},
        {"timeout_seconds": 3601},
        {"cwd": "relative"},
    ],
)
def test_invalid_requests_rejected(tmp_path, overrides):
    value = request("true", tmp_path)
    value.update(overrides)
    with pytest.raises(ValueError):
        worker.execute(value)
