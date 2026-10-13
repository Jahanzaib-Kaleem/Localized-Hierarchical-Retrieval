#!/usr/bin/env python3
"""Bounded, read-only DuckDB queries against LHR's attached Apollo Parquet shards.

The file paths and column names are constrained to the operator-verified manifest
and schema in the fixed data directory; client values are SQL parameters.
"""
import json
import os
from pathlib import Path
import re
import sys
import threading
import time

import duckdb

BASE = Path(os.environ.get("LHR_PARQUET_ROOT", "/data/control/sandbox/apollo-import-source"))
SHARD = re.compile(r"apollo_data_[0-9]{4}\.parquet\Z")
MAX_OUTPUT_ROWS = 10_000
MAX_INPUT_BYTES = 256 * 1024


def identifier(name):
    return '"' + name.replace('"', '""') + '"'


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def source_files(manifest):
    paths = []
    offsets = {}
    total = 0
    for item in manifest["files"]:
        name = item["name"]
        if not isinstance(name, str) or not SHARD.fullmatch(name):
            raise ValueError("invalid Parquet shard filename")
        path = BASE / name
        if not path.is_file():
            raise ValueError("missing Parquet shard " + name)
        offsets[str(path)] = total
        total += int(item["rows"])
        paths.append(str(path))
    if len(paths) != 27 or total != int(manifest["row_count"]):
        raise ValueError("Parquet shard count/row metadata mismatch")
    return paths, offsets


def row_id_expression(offsets):
    cases = " ".join(
        f"WHEN {literal(file_name)} THEN {offset}"
        for file_name, offset in offsets.items()
    )
    return f"(CASE filename {cases} ELSE NULL END + file_row_number)"


def filter_sql(filters, columns):
    clauses = []
    values = []
    for flt in filters:
        col = flt.get("column")
        op = flt.get("op")
        if col not in columns:
            raise ValueError("unknown filter column")
        field = identifier(col)
        # These two native columns are not VARCHAR in the Parquet source.
        # All columns appear as text to LHR's attached schema.
        if col in ("Founded Year", "Date Removed"):
            field = f"CAST({field} AS VARCHAR)"
        if op == "eq":
            val = flt.get("value")
            if val is None:
                clauses.append(f"{field} IS NULL")
            else:
                clauses.append(f"{field} = ?")
                values.append(str(val))
        elif op == "in":
            options = flt.get("values")
            if not isinstance(options, list) or not 1 <= len(options) <= 1024:
                raise ValueError("IN requires 1..1024 values")
            nonnull = [str(v) for v in options if v is not None]
            parts = []
            if nonnull:
                parts.append(f"{field} IN ({','.join('?' for _ in nonnull)})")
                values.extend(nonnull)
            if len(nonnull) != len(options):
                parts.append(f"{field} IS NULL")
            clauses.append("(" + " OR ".join(parts) + ")")
        elif op == "range":
            start = flt.get("gte")
            end = flt.get("lte")
            if start is None and end is None:
                raise ValueError("range requires gte or lte")
            if start is not None:
                clauses.append(f"{field} >= ?")
                values.append(str(start))
            if end is not None:
                clauses.append(f"{field} <= ?")
                values.append(str(end))
        else:
            raise ValueError("unknown filter operation")
    return clauses, values


def query(connection, request, manifest, columns):
    files, offsets = source_files(manifest)
    selections = request.get("select") or columns
    if not isinstance(selections, list) or any(c not in columns for c in selections):
        raise ValueError("unknown selected column")
    if len(selections) > 114:
        raise ValueError("too many selected columns")
    limit = int(request.get("limit") or 100)
    if not 1 <= limit <= MAX_OUTPUT_ROWS:
        raise ValueError("limit exceeds attached bucket bound")
    after = request.get("after_row_id")
    if after is not None and (not isinstance(after, int) or after < 0):
        raise ValueError("invalid after_row_id")
    filters = request.get("filters") or []
    if not isinstance(filters, list) or len(filters) > 32:
        raise ValueError("too many filters")

    source = "[" + ",".join(literal(f) for f in files) + "]"
    from_sql = f"read_parquet({source}, filename=true, file_row_number=true)"
    row_expr = row_id_expression(offsets)
    clauses, values = filter_sql(filters, columns)
    if after is not None:
        clauses.append(f"{row_expr} > ?")
        values.append(after)
    select_sql = ", ".join(identifier(c) for c in selections)
    prefix = f"{row_expr} AS __lhr_row_id"
    if select_sql:
        prefix += ", " + select_sql
    statement = f"SELECT {prefix} FROM {from_sql}"
    if clauses:
        statement += " WHERE " + " AND ".join(f"({x})" for x in clauses)
    statement += f" ORDER BY __lhr_row_id LIMIT {limit+1}"
    started = time.monotonic()
    rows = connection.execute(statement, values).fetchall()
    more = len(rows) > limit
    rows = rows[:limit]
    formatted = []
    for values_row in rows:
        rid = int(values_row[0])
        fields = []
        for col, value in zip(selections, values_row[1:]):
            fields.append({"column": col, "value": None if value is None else str(value)})
        formatted.append({"row_id": rid, "values": fields})
    return {
        "rows": formatted,
        "returned": len(formatted),
        "next_cursor": formatted[-1]["row_id"] if formatted and more else None,
        "stats": {
            "provider": "attached_parquet",
            "elapsed_micros": int((time.monotonic() - started)*1_000_000),
            "hierarchy_lookups": 0,
            "optimized_equality_route": False,
            "rows_examined": None,
            "hits": None,
            "hit_count_exact": False,
            "pages_touched": None,
            "note": "Direct Parquet scan; native LHR exact indexes are not yet built"
        }
    }


def single_row(connection, request, manifest, columns):
    files, offsets = source_files(manifest)
    rid = int(request["row_id"])
    if rid < 0 or rid >= int(manifest["row_count"]):
        return {"bucket": request["bucket"], "row_id": rid, "found": False}
    selected_path = None
    local_id = None
    for path in files:
        start = offsets[path]
        length = next(int(f["rows"]) for f in manifest["files"] if (BASE/f["name"]) == Path(path))
        if start <= rid < start + length:
            selected_path = path
            local_id = rid - start
            break
    if selected_path is None:
        raise ValueError("could not resolve requested row")
    statement = (
        f"SELECT {', '.join(identifier(c) for c in columns)} "
        f"FROM read_parquet({literal(selected_path)}, filename=true, file_row_number=true) "
        "WHERE file_row_number = ? LIMIT 1"
    )
    rows = connection.execute(statement, [local_id]).fetchall()
    if not rows:
        return {"bucket": request["bucket"], "row_id": rid, "found": False}
    return {
        "bucket": request["bucket"], "row_id": rid, "found": True,
        "values": {name: (None if value is None else str(value)) for name, value in zip(columns, rows[0])}
    }


def run():
    payload = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
    if len(payload) > MAX_INPUT_BYTES:
        raise ValueError("query request exceeds input bound")
    request = json.loads(payload)
    manifest = json.loads((BASE / "apollo-manifest.json").read_text())
    schema = json.loads((BASE / "apollo-schema.json").read_text())
    if manifest.get("format") != "LHR-ATTACHED-PARQUET/1":
        raise ValueError("unsupported attached Parquet manifest")
    columns = [c["name"] for c in schema["columns"]]
    if len(columns) != 114 or len(columns) != manifest["columns"]:
        raise ValueError("attached schema changed")
    con = duckdb.connect(config={"threads": 1, "memory_limit": "300MB", "preserve_insertion_order": True})
    timeout = max(1, min(120_000, int(request.get("timeout_ms") or 30_000)))
    timer = threading.Timer(timeout / 1000, con.interrupt)
    timer.daemon = True
    timer.start()
    try:
        if request.get("operation") == "query":
            result = query(con, request, manifest, columns)
        elif request.get("operation") == "row":
            result = single_row(con, request, manifest, columns)
        else:
            raise ValueError("invalid attached bucket operation")
        print(json.dumps(result, ensure_ascii=False, default=str))
    finally:
        timer.cancel()
        con.close()


if __name__ == "__main__":
    try:
        run()
    except Exception as error:
        print(json.dumps({"error": str(error)}))
        sys.exit(1)
