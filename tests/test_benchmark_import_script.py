import py_compile
from pathlib import Path


def test_import_benchmark_script_compiles():
    py_compile.compile(
        str(Path("scripts") / "benchmark_import_jobs.py"),
        doraise=True,
    )
