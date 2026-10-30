"""Exercise the scheduler in a separate process with a disposable SQLite profile."""

import json
import os
from contextlib import closing
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

executable = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="datacraft-scheduler-") as temp:
    root = Path(temp)
    profile = root / "DataCraft"
    profile.mkdir()
    sample = root / "sample.sqlite"
    with closing(sqlite3.connect(sample)) as db, db:
        db.executescript("CREATE TABLE records(value INTEGER); INSERT INTO records VALUES (0);")
    identity = uuid.uuid4().int % 1000000000000000 + 1000000000000000
    (profile / "connections.json").write_text(
        json.dumps([{"id": identity, "name": "test", "driver": "Sqlite", "file_path": str(sample), "auth": "NoAuth"}]),
        encoding="utf-8",
    )
    env = os.environ.copy()
    env["DATACRAFT_TEST_DATA_DIR"] = str(profile)
    flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    worker = subprocess.Popen([str(executable), "--scheduler"], env=env, creationflags=flags)
    try:
        schedule = profile / "scheduler.sqlite"
        deadline = time.monotonic() + 20
        while not schedule.exists() and time.monotonic() < deadline:
            assert worker.poll() is None, f"worker exited: {worker.returncode}"
            time.sleep(0.1)
        assert schedule.exists(), "scheduler database was not created"
        deadline = time.monotonic() + 10
        while True:
            try:
                with closing(sqlite3.connect(schedule)) as db, db:
                    db.execute(
                        "INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run) VALUES (?, ?, ?, ?, ?, 1, ?)",
                        ("test", identity, "main", "UPDATE records SET value=value+1", 3, int(time.time())),
                    )
                break
            except sqlite3.OperationalError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
        duplicate = subprocess.run([str(executable), "--scheduler"], env=env, creationflags=flags, timeout=10)
        assert duplicate.returncode == 0, "second scheduler did not exit cleanly"
        deadline = time.monotonic() + 20
        runs = []
        while time.monotonic() < deadline:
            assert worker.poll() is None, f"worker exited: {worker.returncode}"
            with closing(sqlite3.connect(schedule)) as db:
                runs = db.execute("SELECT status, message FROM runs").fetchall()
            if len(runs) >= 2 and all(status != "running" for status, _ in runs):
                break
            time.sleep(0.1)
        assert runs == [("succeeded", ""), ("succeeded", "")], runs
        with closing(sqlite3.connect(schedule)) as db, db:
            db.execute("UPDATE jobs SET enabled=0")
        with closing(sqlite3.connect(sample)) as db:
            assert db.execute("SELECT value FROM records").fetchone() == (2,)
    finally:
        worker.terminate()
        worker.wait(timeout=10)
