"""Localized Hierarchical Retrieval reference engine.

v0 intentionally favors clarity and correctness over API stability.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping
import json
import numpy as np


@dataclass(frozen=True)
class Predicate:
    column: int
    value: int


class Dataset:
    """Read-only memory-mapped LHR/0 dataset."""

    def __init__(self, path: str | Path):
        self.path = Path(path)
        with (self.path / "manifest.json").open("r", encoding="utf-8") as f:
            self.manifest = json.load(f)
        self.rows = np.load(self.path / "canonical" / "rows.npy", mmap_mode="r")
        self.signatures = np.load(self.path / "routing" / "signatures.npy", mmap_mode="r")
        self.starts = np.load(self.path / "routing" / "starts.npy", mmap_mode="r")
        self.lengths = np.load(self.path / "routing" / "lengths.npy", mmap_mode="r")

    def query(self, predicates: Iterable[Predicate]) -> np.ndarray:
        predicates = tuple(predicates)
        if not predicates:
            return np.arange(len(self.rows), dtype=np.uint32)

        possible = np.ones(len(self.signatures), dtype=bool)
        for p in predicates:
            if p.value < 0 or p.value >= 32:
                # v0 signature is uint32. Values outside it cannot be represented.
                # Fall back to exact canonical checking rather than risk a false negative.
                possible[:] = True
                break
            possible &= ((self.signatures[:, p.column] >> np.uint32(p.value)) & 1).astype(bool)

        hits: list[np.ndarray] = []
        for page_id in np.flatnonzero(possible):
            start = int(self.starts[page_id])
            end = start + int(self.lengths[page_id])
            block = self.rows[start:end]
            keep = np.ones(len(block), dtype=bool)
            for p in predicates:
                keep &= block[:, p.column] == p.value
            if keep.any():
                hits.append(np.arange(start, end, dtype=np.uint32)[keep])

        return np.concatenate(hits) if hits else np.empty(0, dtype=np.uint32)


def build_dataset(rows: np.ndarray, path: str | Path, page_rows: int = 256) -> None:
    """Build the simple exact LHR/0 page-signature format.

    This is a baseline writer. More advanced adaptive/hierarchical builders will
    replace routing internals without changing the correctness contract.
    """
    if rows.ndim != 2:
        raise ValueError("rows must be a 2D integer matrix")
    if rows.min(initial=0) < 0:
        raise ValueError("v0 requires non-negative integer tokens")

    path = Path(path)
    (path / "canonical").mkdir(parents=True, exist_ok=True)
    (path / "routing").mkdir(parents=True, exist_ok=True)

    np.save(path / "canonical" / "rows.npy", rows)

    starts = np.arange(0, len(rows), page_rows, dtype=np.uint32)
    lengths = np.minimum(page_rows, len(rows) - starts.astype(np.int64)).astype(np.uint32)
    signatures = np.zeros((len(starts), rows.shape[1]), dtype=np.uint32)

    for i, (start, length) in enumerate(zip(starts, lengths)):
        block = rows[int(start): int(start) + int(length)]
        for col in range(rows.shape[1]):
            values = np.unique(block[:, col])
            values = values[(values >= 0) & (values < 32)].astype(np.uint32)
            if len(values):
                signatures[i, col] = np.bitwise_or.reduce(np.left_shift(np.uint32(1), values))

    np.save(path / "routing" / "signatures.npy", signatures)
    np.save(path / "routing" / "starts.npy", starts)
    np.save(path / "routing" / "lengths.npy", lengths)

    manifest = {
        "format": "LHR/0",
        "rows": int(rows.shape[0]),
        "columns": int(rows.shape[1]),
        "canonical_dtype": str(rows.dtype),
        "page_rows": int(page_rows),
        "router": "uint32-presence-signature",
    }
    with (path / "manifest.json").open("w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2)
