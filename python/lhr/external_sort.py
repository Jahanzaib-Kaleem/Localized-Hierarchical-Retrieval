"""Bounded-memory external sort primitives for LHR hierarchy construction."""
from __future__ import annotations
from pathlib import Path
import heapq
import os
import struct
import tempfile
from typing import Iterable, Iterator

_RECORD = struct.Struct("<QI")  # uint64 hierarchy key, uint32 page id


def write_sorted_run(records: list[tuple[int, int]], directory: Path) -> Path:
    records.sort()
    fd, name = tempfile.mkstemp(prefix="lhr-run-", suffix=".bin", dir=directory)
    with os.fdopen(fd, "wb") as f:
        last = None
        for record in records:
            if record != last:
                f.write(_RECORD.pack(*record))
                last = record
    return Path(name)


def iter_run(path: Path) -> Iterator[tuple[int, int]]:
    with path.open("rb", buffering=1024 * 1024) as f:
        while True:
            raw = f.read(_RECORD.size)
            if not raw:
                return
            if len(raw) != _RECORD.size:
                raise IOError(f"truncated LHR run: {path}")
            yield _RECORD.unpack(raw)


def merge_runs(runs: Iterable[Path], output: Path) -> int:
    """K-way merge sorted runs while deduplicating identical records."""
    streams = [iter_run(p) for p in runs]
    merged = heapq.merge(*streams)
    count = 0
    last = None
    with output.open("wb", buffering=1024 * 1024) as f:
        for record in merged:
            if record == last:
                continue
            f.write(_RECORD.pack(*record))
            last = record
            count += 1
    return count


def external_sort(records: Iterable[tuple[int, int]], output: Path, *, max_records: int = 500_000) -> int:
    """Sort arbitrary hierarchy records with memory bounded by max_records."""
    output.parent.mkdir(parents=True, exist_ok=True)
    runs: list[Path] = []
    buf: list[tuple[int, int]] = []
    try:
        for record in records:
            buf.append(record)
            if len(buf) >= max_records:
                runs.append(write_sorted_run(buf, output.parent))
                buf = []
        if buf:
            runs.append(write_sorted_run(buf, output.parent))
        if not runs:
            output.write_bytes(b"")
            return 0
        return merge_runs(runs, output)
    finally:
        for p in runs:
            p.unlink(missing_ok=True)
