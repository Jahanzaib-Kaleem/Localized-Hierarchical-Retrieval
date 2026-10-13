"""Exercise real read-only queries against 27 Parquet shards without duplicating them."""
import importlib.util
from pathlib import Path

import duckdb
import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "parquet_bucket_query.py"
SPEC = importlib.util.spec_from_file_location("parquet_bucket_query", SCRIPT)
assert SPEC and SPEC.loader
worker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(worker)


@pytest.fixture
def shards(tmp_path, monkeypatch):
    monkeypatch.setattr(worker, "BASE", tmp_path)
    con = duckdb.connect()
    files = []
    for i in range(27):
        name = f"apollo_data_{i:04d}.parquet"
        path = tmp_path / name
        con.execute(
            f"COPY (SELECT 'person{i}@example.com' AS Email, 'Role' AS Title) "
            f"TO '{path.as_posix()}' (FORMAT PARQUET, COMPRESSION ZSTD)"
        )
        files.append({"name": name, "rows": 1, "bytes": path.stat().st_size})
    yield con, {"format":worker.__name__,"files":files,"row_count":27,"columns":2}
    con.close()


def test_filter_and_global_row_ids(shards):
    con, manifest = shards
    request = {
        "filters": [{"op":"eq","column":"Email","value":"person18@example.com"}],
        "select": ["Email"],
        "limit": 10,
        "timeout_ms": 30000,
    }
    result = worker.query(con, request, manifest, ["Email", "Title"])
    assert result["returned"] == 1
    assert result["rows"][0]["row_id"] == 18
    assert result["rows"][0]["values"][0]["value"] == "person18@example.com"


def test_cursor_pagination(shards):
    con, manifest = shards
    request = {"filters":[],"select":["Title"],"limit":3}
    first = worker.query(con, request, manifest, ["Email","Title"])
    assert [r["row_id"] for r in first["rows"]] == [0,1,2]
    assert first["next_cursor"] == 2
    second = worker.query(con, {**request,"after_row_id":first["next_cursor"]},manifest,["Email","Title"])
    assert [r["row_id"] for r in second["rows"]] == [3,4,5]


def test_single_row_last_shard(shards):
    con, manifest = shards
    result = worker.single_row(con, {"row_id":26,"bucket":"apollo-full-50m"},manifest,["Email","Title"])
    assert result["found"]
    assert result["row_id"] == 26
    assert result["values"]["Email"] == "person26@example.com"


def test_unknown_columns_are_rejected(shards):
    con, manifest = shards
    with pytest.raises(ValueError, match="unknown selected column"):
        worker.query(con, {"filters":[],"select":["Missing"],"limit":2},manifest,["Email","Title"])
    with pytest.raises(ValueError, match="unknown filter column"):
        worker.query(con, {"filters":[{"op":"eq","column":"Missing","value":"x"}],"limit":2},manifest,["Email","Title"])


def test_filter_values_are_bound_not_interpolated(shards):
    con, manifest = shards
    result=worker.query(con,{"filters":[{"op":"eq","column":"Email","value":"' OR TRUE --"}],"select":["Email"],"limit":1},manifest,["Email","Title"])
    assert result["returned"] == 0
