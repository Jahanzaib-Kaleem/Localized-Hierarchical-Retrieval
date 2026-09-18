#!/usr/bin/env python3
"""On-demand end-to-end benchmark for LHR's chunked Studio import-job protocol.

This script deliberately does not run in normal CI. It generates CSV data incrementally on disk,
uploads it through the same 4 MiB job API used by Studio, samples process/disk metrics, verifies
row counts and exact lookups, and cleans benchmark buckets by default.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

CHUNK_BYTES = 4 * 1024 * 1024
FINGERPRINT_BYTES = 1024 * 1024

SCHEMA = {
    "format": "LHR-SCHEMA/1",
    "columns": [
        {
            "name": "id",
            "logical_type": "unsigned",
            "nullable": False,
            "normalization": "trim",
            "null_values": [],
        },
        {
            "name": "segment",
            "logical_type": "text",
            "nullable": False,
            "normalization": "trim_lowercase",
            "null_values": [],
        },
        {
            "name": "visits",
            "logical_type": "unsigned",
            "nullable": False,
            "normalization": "trim",
            "null_values": [],
        },
        {
            "name": "email",
            "logical_type": "text",
            "nullable": False,
            "normalization": "trim_lowercase",
            "null_values": [],
        },
        {
            "name": "active",
            "logical_type": "boolean",
            "nullable": False,
            "normalization": "trim",
            "null_values": [],
        },
    ],
}


class ApiFailure(RuntimeError):
    pass


def api_request(
    base_url: str,
    token: str,
    method: str,
    path: str,
    *,
    payload: Any | None = None,
    body: bytes | None = None,
    content_type: str | None = None,
    timeout: float = 120.0,
) -> Any:
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = body
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    elif content_type:
        headers["Content-Type"] = content_type
    request = urllib.request.Request(
        base_url.rstrip("/") + path,
        data=data,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            content_type_value = response.headers.get("content-type", "")
            if "application/json" in content_type_value:
                return json.loads(raw)
            return raw.decode()
    except urllib.error.HTTPError as error:
        raw = error.read().decode(errors="replace")
        try:
            parsed = json.loads(raw)
            detail = parsed.get("error", raw)
        except json.JSONDecodeError:
            detail = raw
        raise ApiFailure(f"{method} {path} -> HTTP {error.code}: {detail}") from error
    except urllib.error.URLError as error:
        raise ApiFailure(f"{method} {path} failed: {error}") from error


def generate_csv(path: Path, target_bytes: int, first_id: int) -> tuple[int, int]:
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = 0
    with path.open("wb", buffering=1024 * 1024) as output:
        header = b"id,segment,visits,email,active\n"
        output.write(header)
        written = len(header)
        while written < target_bytes:
            row_id = first_id + rows
            record = (
                f"{row_id},segment-{row_id % 64},{row_id % 100000},"
                f"user-{row_id}@example.test,{'true' if row_id % 3 else 'false'}\n"
            ).encode()
            output.write(record)
            written += len(record)
            rows += 1
        output.flush()
        os.fsync(output.fileno())
    return rows, path.stat().st_size


def fingerprint(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        digest.update(source.read(FINGERPRINT_BYTES))
    return digest.hexdigest()


def parse_metrics(text: str) -> dict[str, int]:
    metrics: dict[str, int] = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2:
            continue
        try:
            metrics[parts[0]] = int(float(parts[1]))
        except ValueError:
            continue
    return metrics


class Sampler:
    def __init__(self, base_url: str, token: str, disk_path: Path):
        self.base_url = base_url
        self.token = token
        self.disk_path = disk_path
        self.peak_rss = 0
        self.start_minor: int | None = None
        self.start_major: int | None = None
        self.start_read: int | None = None
        self.start_write: int | None = None
        self.min_free = shutil.disk_usage(disk_path).free

    def sample(self) -> None:
        text = api_request(self.base_url, self.token, "GET", "/metrics", timeout=30)
        metrics = parse_metrics(text)
        rss = metrics.get("lhr_process_rss_bytes", 0)
        self.peak_rss = max(self.peak_rss, rss)
        minor = metrics.get("lhr_process_minor_page_faults_total")
        major = metrics.get("lhr_process_major_page_faults_total")
        read = metrics.get("lhr_process_read_bytes_total")
        write = metrics.get("lhr_process_write_bytes_total")
        if self.start_minor is None:
            self.start_minor = minor
            self.start_major = major
            self.start_read = read
            self.start_write = write
        self.min_free = min(self.min_free, shutil.disk_usage(self.disk_path).free)

    def finish(self) -> dict[str, int | None]:
        self.sample()
        text = api_request(self.base_url, self.token, "GET", "/metrics", timeout=30)
        metrics = parse_metrics(text)

        def delta(name: str, start: int | None) -> int | None:
            value = metrics.get(name)
            if value is None or start is None:
                return None
            return max(0, value - start)

        return {
            "peak_rss_bytes_observed": self.peak_rss,
            "minor_page_faults_delta": delta(
                "lhr_process_minor_page_faults_total", self.start_minor
            ),
            "major_page_faults_delta": delta(
                "lhr_process_major_page_faults_total", self.start_major
            ),
            "process_read_bytes_delta": delta(
                "lhr_process_read_bytes_total", self.start_read
            ),
            "process_write_bytes_delta": delta(
                "lhr_process_write_bytes_total", self.start_write
            ),
            "minimum_filesystem_free_bytes_observed": self.min_free,
        }


def create_bucket(base_url: str, token: str, bucket: str) -> None:
    api_request(
        base_url,
        token,
        "POST",
        "/v1/admin/buckets/create",
        payload={"id": bucket, "name": bucket},
    )


def delete_bucket(base_url: str, token: str, bucket: str) -> None:
    api_request(
        base_url,
        token,
        "POST",
        "/v1/admin/buckets/delete",
        payload={"id": bucket, "confirm": bucket},
    )


def create_job(
    base_url: str,
    token: str,
    bucket: str,
    mode: str,
    path: Path,
) -> dict[str, Any]:
    response = api_request(
        base_url,
        token,
        "POST",
        "/v1/admin/imports",
        payload={
            "bucket": bucket,
            "mode": mode,
            "schema": SCHEMA,
            "file_name": path.name,
            "file_fingerprint": fingerprint(path),
            "bytes_total": path.stat().st_size,
        },
    )
    return response["result"]


def upload_job(
    base_url: str,
    token: str,
    path: Path,
    job: dict[str, Any],
    sampler: Sampler,
) -> tuple[dict[str, Any], float]:
    started = time.monotonic()
    offset = int(job["bytes_received"])
    chunk_number = 0
    with path.open("rb") as source:
        source.seek(offset)
        while offset < path.stat().st_size:
            chunk = source.read(CHUNK_BYTES)
            if not chunk:
                raise RuntimeError("CSV ended before the declared size")
            response = api_request(
                base_url,
                token,
                "PUT",
                f"/v1/admin/imports/{urllib.parse.quote(job['id'])}/chunk?offset={offset}",
                body=chunk,
                content_type="application/octet-stream",
            )
            job = response["result"]
            offset = int(job["bytes_received"])
            chunk_number += 1
            if chunk_number == 1 or chunk_number % 8 == 0:
                sampler.sample()
            percent = 100.0 * offset / int(job["bytes_total"])
            print(
                f"upload {job['id']}: {offset:,}/{int(job['bytes_total']):,} "
                f"bytes ({percent:.1f}%)",
                file=sys.stderr,
            )
    return job, time.monotonic() - started


def complete_and_wait(
    base_url: str,
    token: str,
    job: dict[str, Any],
    sampler: Sampler,
    timeout_seconds: float,
) -> tuple[dict[str, Any], float]:
    started = time.monotonic()
    response = api_request(
        base_url,
        token,
        "POST",
        f"/v1/admin/imports/{urllib.parse.quote(job['id'])}/complete",
    )
    job = response["result"]
    deadline = time.monotonic() + timeout_seconds
    last_stage = None
    while True:
        if time.monotonic() > deadline:
            raise TimeoutError(f"import job {job['id']} exceeded benchmark timeout")
        response = api_request(
            base_url,
            token,
            "GET",
            f"/v1/admin/imports/{urllib.parse.quote(job['id'])}",
            timeout=30,
        )
        job = response["result"]
        sampler.sample()
        stage = job["stage"]
        if stage != last_stage or job.get("rows_parsed") is not None:
            rows = job.get("rows_parsed")
            suffix = f" rows_parsed={rows:,}" if isinstance(rows, int) else ""
            print(f"build {job['id']}: {stage}{suffix}", file=sys.stderr)
            last_stage = stage
        if job["status"] == "complete":
            return job, time.monotonic() - started
        if job["status"] == "failed":
            raise ApiFailure(
                f"import job {job['id']} failed in {job['stage']}: {job.get('error')}"
            )
        time.sleep(1.0)


def verify_bucket(
    base_url: str,
    token: str,
    bucket: str,
    expected_rows: int,
    first_id: int,
    last_id: int,
) -> dict[str, Any]:
    stats = api_request(
        base_url,
        token,
        "GET",
        f"/v1/stats?bucket={urllib.parse.quote(bucket)}",
    )["result"]
    if int(stats["rows"]) != expected_rows:
        raise AssertionError(
            f"row count mismatch: expected {expected_rows}, got {stats['rows']}"
        )
    for row_id in (first_id, last_id):
        response = api_request(
            base_url,
            token,
            "POST",
            "/v1/query",
            payload={
                "bucket": bucket,
                "filters": [{"op": "eq", "column": "id", "value": str(row_id)}],
                "select": ["id", "email"],
                "limit": 10,
                "after_row_id": None,
                "max_rows_examined": 1000,
                "timeout_ms": 5000,
            },
        )["result"]
        if int(response["stats"]["hits"]) != 1:
            raise AssertionError(
                f"exact lookup for id={row_id} returned {response['stats']['hits']} hits"
            )
    return stats


def run_part(
    args: argparse.Namespace,
    bucket: str,
    mode: str,
    size_mb: int,
    first_id: int,
    expected_rows_before: int,
    index: int,
) -> tuple[dict[str, Any], int]:
    target_bytes = size_mb * 1024 * 1024
    path = args.workdir / f"{bucket}-part-{index:02d}-{size_mb}mb.csv"
    generated_rows, source_bytes = generate_csv(path, target_bytes, first_id)
    sampler = Sampler(args.base_url, args.token, args.workdir)
    sampler.sample()
    started = time.monotonic()
    try:
        job = create_job(args.base_url, args.token, bucket, mode, path)
        job, upload_seconds = upload_job(args.base_url, args.token, path, job, sampler)
        job, build_seconds = complete_and_wait(
            args.base_url,
            args.token,
            job,
            sampler,
            args.timeout_minutes * 60,
        )
        final_expected = expected_rows_before + generated_rows
        stats = verify_bucket(
            args.base_url,
            args.token,
            bucket,
            final_expected,
            first_id,
            first_id + generated_rows - 1,
        )
        observed = sampler.finish()
        report = {
            "mode": mode,
            "bucket": bucket,
            "target_mb": size_mb,
            "source_csv_bytes": source_bytes,
            "generated_rows": generated_rows,
            "columns": len(SCHEMA["columns"]),
            "rows_before": expected_rows_before,
            "rows_after": final_expected,
            "upload_seconds": round(upload_seconds, 3),
            "server_build_seconds": round(build_seconds, 3),
            "total_seconds": round(time.monotonic() - started, 3),
            "final_dataset_bytes": int(stats["total_bytes"]),
            "job_result": job.get("result"),
            **observed,
        }
        print(json.dumps(report, sort_keys=True))
        return report, generated_rows
    finally:
        if not args.keep_files:
            try:
                path.unlink()
            except FileNotFoundError:
                pass


def fresh_create_suite(args: argparse.Namespace) -> list[dict[str, Any]]:
    reports = []
    stamp = int(time.time())
    for index, size_mb in enumerate(args.sizes_mb):
        bucket = f"{args.bucket_prefix}-{size_mb}-{stamp}-{index}"
        create_bucket(args.base_url, args.token, bucket)
        try:
            report, _ = run_part(args, bucket, "create", size_mb, 0, 0, index)
            reports.append(report)
        finally:
            if not args.keep_buckets:
                delete_bucket(args.base_url, args.token, bucket)
    return reports


def append_suite(args: argparse.Namespace) -> list[dict[str, Any]]:
    reports = []
    stamp = int(time.time())
    bucket = f"{args.bucket_prefix}-append-{stamp}"
    create_bucket(args.base_url, args.token, bucket)
    expected_rows = 0
    next_id = 0
    try:
        for index, size_mb in enumerate(args.append_parts_mb):
            mode = "create" if index == 0 else "append"
            report, generated = run_part(
                args,
                bucket,
                mode,
                size_mb,
                next_id,
                expected_rows,
                index,
            )
            reports.append(report)
            expected_rows += generated
            next_id += generated
    finally:
        if not args.keep_buckets:
            delete_bucket(args.base_url, args.token, bucket)
    return reports


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--base-url",
        default=os.environ.get("LHR_BASE_URL", "http://127.0.0.1:8787"),
    )
    parser.add_argument("--token", default=os.environ.get("LHR_API_TOKEN", ""))
    parser.add_argument(
        "--workdir",
        type=Path,
        default=Path("/data/temp/import-benchmarks"),
        help="Place generated sources on the large data volume, not the boot disk.",
    )
    parser.add_argument("--bucket-prefix", default="import-bench")
    parser.add_argument("--timeout-minutes", type=float, default=120.0)
    parser.add_argument("--keep-files", action="store_true")
    parser.add_argument("--keep-buckets", action="store_true")
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument(
        "--sizes-mb",
        type=int,
        nargs="+",
        help="Fresh create benchmark for each size, e.g. 50 250 700 1024.",
    )
    modes.add_argument(
        "--append-parts-mb",
        type=int,
        nargs="+",
        help="Create with part 1, then append every later part to the same bucket.",
    )
    args = parser.parse_args()
    values = args.sizes_mb or args.append_parts_mb
    if any(value <= 0 for value in values):
        parser.error("all benchmark sizes must be positive")
    args.workdir.mkdir(parents=True, exist_ok=True)
    return args


def main() -> int:
    args = parse_args()
    api_request(args.base_url, args.token, "GET", "/healthz", timeout=10)
    started_free = shutil.disk_usage(args.workdir).free
    reports = fresh_create_suite(args) if args.sizes_mb else append_suite(args)
    summary = {
        "base_url": args.base_url,
        "started_free_bytes": started_free,
        "finished_free_bytes": shutil.disk_usage(args.workdir).free,
        "reports": reports,
    }
    print(json.dumps(summary, indent=2, sort_keys=True), file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
