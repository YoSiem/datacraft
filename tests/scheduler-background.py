"""Exercise SQL and recurring backups without a GUI, using a disposable profile."""

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
    backups = root / "backups"
    backups.mkdir()
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
                    sql_id = db.execute(
                        "INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run) VALUES (?, ?, ?, ?, ?, 1, ?)",
                        ("test", identity, "main", "UPDATE records SET value=value+1", 3, int(time.time())),
                    ).lastrowid
                    backup_id = db.execute(
                        "INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run, backup_json) VALUES (?, ?, ?, '', ?, 1, ?, ?)",
                        (
                            "backup", identity, "main", 3, int(time.time()),
                            json.dumps({"folder": str(backups), "utility": "", "compress": False, "archive": False, "directory": ""}),
                        ),
                    ).lastrowid
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
                runs = db.execute("SELECT job_id, status, message FROM runs ORDER BY id").fetchall()
            if all(sum(job == job_id and status == "succeeded" for job, status, _ in runs) >= 2 for job_id in (sql_id, backup_id)):
                break
            time.sleep(0.1)
        with closing(sqlite3.connect(schedule)) as db, db:
            db.execute("UPDATE jobs SET enabled=0")
        deadline = time.monotonic() + 10
        while True:
            with closing(sqlite3.connect(schedule)) as db:
                runs = db.execute("SELECT job_id, status, message FROM runs ORDER BY id").fetchall()
            if all(status != "running" for _, status, _ in runs):
                break
            assert time.monotonic() < deadline, runs
            time.sleep(0.1)
        assert all(status == "succeeded" for _, status, _ in runs), runs
        sql_runs = [message for job, _, message in runs if job == sql_id]
        destinations = [Path(message) for job, _, message in runs if job == backup_id]
        assert len(sql_runs) >= 2 and all(message == "" for message in sql_runs), runs
        assert len(destinations) >= 2 and len(set(destinations)) == len(destinations), runs
        assert set(backups.iterdir()) == set(destinations), runs
        for destination in destinations:
            assert destination.parent == backups and destination.suffix == ".sqlite", destination
            with closing(sqlite3.connect(destination)) as db:
                value, = db.execute("SELECT value FROM records").fetchone()
                assert 0 <= value <= len(sql_runs), value
        with closing(sqlite3.connect(sample)) as db:
            assert db.execute("SELECT value FROM records").fetchone() == (len(sql_runs),)
    finally:
        worker.terminate()
        worker.wait(timeout=10)
