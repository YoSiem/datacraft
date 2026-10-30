//! Durable SQL schedules shared by the application and its background process.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::time::Duration;

pub mod dialog;
mod service;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn database_path() -> anyhow::Result<std::path::PathBuf> {
    let dir = crate::config::data_dir();
    std::fs::create_dir_all(&dir).context("create scheduler data directory")?;
    Ok(dir.join("scheduler.sqlite"))
}

/// Run in a separate, per-user process. The OS holds this lock for the process lifetime,
/// including when the GUI is closed, and releases it after a crash.
pub fn run_background() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let path = database_path()?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .context("open scheduler lock")?;
    // The GUI may briefly acquire the same lock while checking whether the scheduler is running.
    let mut acquired = false;
    for _ in 0..20 {
        match lock.try_lock() {
            Ok(()) => {
                acquired = true;
                break;
            }
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("lock scheduler");
            }
        }
    }
    // Another instance owns the scheduler; never mark its runs interrupted.
    if !acquired {
        return Ok(());
    }
    let mut store = Store::open(&path)?;
    store.interrupt_abandoned(now())?;
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let due = if active.load(Ordering::SeqCst) < 4 {
            match store.claim_due(now()) {
                Ok(due) => due,
                Err(error) => {
                    eprintln!("could not load due SQL tasks: {error:#}");
                    std::thread::sleep(Duration::from_secs(10));
                    continue;
                }
            }
        } else {
            None
        };
        if let Some((job, run_id)) = due {
            active.fetch_add(1, Ordering::SeqCst);
            let worker_active = Arc::clone(&active);
            let path = path.clone();
            let spawned = std::thread::Builder::new()
                .name("sql-scheduled-job".into())
                .spawn(move || {
                    let message = match std::panic::catch_unwind(|| execute(&job)) {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(format!("{error:#}")),
                        Err(_) => Some("SQL task stopped unexpectedly".into()),
                    };
                    loop {
                        match Store::open(&path)
                            .and_then(|mut store| store.finish(run_id, now(), message.as_deref()))
                        {
                            Ok(()) => break,
                            Err(error) => {
                                eprintln!("could not record SQL job result: {error:#}");
                                std::thread::sleep(Duration::from_secs(10));
                            }
                        }
                    }
                    worker_active.fetch_sub(1, Ordering::SeqCst);
                });
            if let Err(error) = spawned {
                active.fetch_sub(1, Ordering::SeqCst);
                store.finish(
                    run_id,
                    now(),
                    Some(&format!("Could not start SQL task: {error}")),
                )?;
            }
            continue;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn execute(job: &Job) -> anyhow::Result<()> {
    let cfg = crate::config::load_background_connection(job.connection)?;
    let mut secrets = vec![
        cfg.password.clone(),
        cfg.ssh.password.clone(),
        cfg.ssh.passphrase.clone(),
        cfg.tls.key_passphrase.clone(),
    ];
    secrets.extend(cfg.secret_properties().into_iter().map(|(_, value)| value));
    secrets.retain(|secret| !secret.is_empty());
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    execute_with_config(job, cfg).map_err(|error| {
        let mut message = format!("{error:#}");
        for secret in secrets {
            message = message.replace(&secret, "[redacted]");
        }
        anyhow::anyhow!(message)
    })
}

fn execute_with_config(job: &Job, mut cfg: crate::db::ConnectionConfig) -> anyhow::Result<()> {
    use crate::db::{Driver, Session};

    if cfg.driver.is_nosql() {
        bail!("This data source does not use SQL");
    }
    let statements = crate::sql::split_statements(&job.sql, cfg.driver);
    if statements.is_empty() {
        bail!("The task contains no SQL statements");
    }
    if cfg.read_only
        && statements
            .iter()
            .any(|statement| !crate::app::is_read_statement(cfg.driver, &statement.text))
    {
        bail!("A read-only data source cannot run a statement that changes data");
    }
    if cfg.ask_password {
        bail!("Save the data source password before running a task in the background");
    }
    if cfg.driver == Driver::Sqlite
        && !job.database.trim().is_empty()
        && job.database.trim() != "main"
    {
        bail!("SQLite tasks can only use the main database");
    }
    if !job.database.trim().is_empty() {
        cfg.database.clone_from(&job.database);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut session = tokio::time::timeout(Duration::from_secs(30), Session::connect(&cfg))
            .await
            .context("Connection timed out")??;
        if !job.database.trim().is_empty() {
            session.use_database(&job.database).await?;
        }
        for statement in statements {
            session.execute(&statement.text, 1).await?;
        }
        Ok(())
    })
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub connection: u64,
    pub database: String,
    pub sql: String,
    pub interval_seconds: i64,
    pub enabled: bool,
    pub next_run: i64,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i64,
    pub started: i64,
    pub finished: Option<i64>,
    pub status: String,
    pub message: String,
}

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)?;
        }
        let db = Connection::open(path)
            .with_context(|| format!("open schedule database {}", path.display()))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS jobs (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 connection INTEGER NOT NULL,
                 database_name TEXT NOT NULL,
                 sql_text TEXT NOT NULL,
                 interval_seconds INTEGER NOT NULL CHECK(interval_seconds > 0),
                 enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
                 next_run INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS runs (
                 id INTEGER PRIMARY KEY,
                 job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                 started INTEGER NOT NULL,
                 finished INTEGER,
                 status TEXT NOT NULL,
                 message TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id, id DESC);",
        )?;
        Ok(Self(db))
    }

    pub fn jobs(&self) -> anyhow::Result<Vec<Job>> {
        let mut stmt = self.0.prepare("SELECT id, name, connection, database_name, sql_text, interval_seconds, enabled, next_run FROM jobs ORDER BY name, id")?;
        let rows = stmt.query_map([], read_job)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn runs(&self, job_id: i64) -> anyhow::Result<Vec<Run>> {
        let mut stmt = self.0.prepare("SELECT id, job_id, started, finished, status, message FROM runs WHERE job_id = ?1 ORDER BY id DESC LIMIT 100")?;
        let rows = stmt.query_map([job_id], |row| {
            Ok(Run {
                id: row.get(0)?,
                started: row.get(2)?,
                finished: row.get(3)?,
                status: row.get(4)?,
                message: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Creating or editing a job starts a new interval; active runs cannot be edited.
    pub fn save(&mut self, job: &Job, now: i64) -> anyhow::Result<i64> {
        validate(job, now)?;
        let connection = i64::try_from(job.connection).context("connection ID is too large")?;
        let next_run = now
            .checked_add(job.interval_seconds)
            .context("interval is too large")?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if job.id == 0 {
            tx.execute("INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![job.name, connection, job.database, job.sql, job.interval_seconds, job.enabled, next_run])?;
            tx.last_insert_rowid()
        } else {
            let changed = tx.execute("UPDATE jobs SET name=?1, connection=?2, database_name=?3, sql_text=?4, interval_seconds=?5, enabled=?6, next_run=?7 WHERE id=?8 AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id=?8 AND status='running')", params![job.name, connection, job.database, job.sql, job.interval_seconds, job.enabled, next_run, job.id])?;
            if changed != 1 {
                bail!("Job is running or no longer exists");
            }
            job.id
        };
        tx.commit()?;
        Ok(id)
    }

    pub fn delete(&mut self, id: i64) -> anyhow::Result<()> {
        let changed = self.0.execute("DELETE FROM jobs WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id=?1 AND status='running')", [id])?;
        if changed != 1 {
            bail!("Job is running or no longer exists");
        }
        Ok(())
    }

    /// Prevent future runs without interrupting a statement already sent to a database.
    pub fn pause(&self, id: i64) -> anyhow::Result<()> {
        if self
            .0
            .execute("UPDATE jobs SET enabled=0 WHERE id=?1", [id])?
            != 1
        {
            bail!("Job no longer exists");
        }
        Ok(())
    }

    /// Claim and advance in one transaction. Missed intervals are skipped, never replayed.
    pub fn claim_due(&mut self, now: i64) -> anyhow::Result<Option<(Job, i64)>> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = tx.query_row("SELECT id, name, connection, database_name, sql_text, interval_seconds, enabled, next_run FROM jobs WHERE enabled=1 AND next_run<=?1 AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id=jobs.id AND status='running') ORDER BY next_run, id LIMIT 1", [now], read_job).optional()?;
        let Some(job) = job else {
            tx.commit()?;
            return Ok(None);
        };
        let next = now
            .checked_add(job.interval_seconds)
            .context("next run time overflow")?;
        tx.execute(
            "UPDATE jobs SET next_run=?1 WHERE id=?2",
            params![next, job.id],
        )?;
        tx.execute(
            "INSERT INTO runs(job_id, started, status) VALUES (?1, ?2, 'running')",
            params![job.id, now],
        )?;
        let run_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some((job, run_id)))
    }

    pub fn finish(&mut self, run_id: i64, now: i64, error: Option<&str>) -> anyhow::Result<()> {
        let status = if error.is_some() {
            "failed"
        } else {
            "succeeded"
        };
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE runs SET finished=?1, status=?2, message=?3 WHERE id=?4 AND status='running'",
            params![now, status, error.unwrap_or(""), run_id],
        )?;
        if changed != 1 {
            bail!("Run is no longer active");
        }
        // A long run gets a full interval after it finishes instead of an immediate rerun.
        tx.execute("UPDATE jobs SET next_run=?1+interval_seconds WHERE id=(SELECT job_id FROM runs WHERE id=?2) AND next_run<=?1 AND ?1<=9223372036854775807-interval_seconds", params![now, run_id])?;
        tx.execute("DELETE FROM runs WHERE job_id=(SELECT job_id FROM runs WHERE id=?1) AND id NOT IN (SELECT id FROM runs WHERE job_id=(SELECT job_id FROM runs WHERE id=?1) ORDER BY id DESC LIMIT 100)", [run_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Call only after acquiring the exclusive scheduler-process lock.
    pub fn interrupt_abandoned(&self, now: i64) -> anyhow::Result<()> {
        self.0.execute("UPDATE runs SET finished=?1, status='interrupted', message='Background process stopped before completion' WHERE status='running'", [now])?;
        Ok(())
    }
}

fn validate(job: &Job, now: i64) -> anyhow::Result<()> {
    if job.name.trim().is_empty()
        || job.sql.trim().is_empty()
        || job.interval_seconds <= 0
        || job.connection == 0
    {
        bail!("A name, data source, SQL and a positive interval are required");
    }
    i64::try_from(job.connection).context("connection ID is too large")?;
    now.checked_add(job.interval_seconds)
        .context("interval is too large")?;
    Ok(())
}

fn read_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let connection: i64 = row.get(2)?;
    let connection = u64::try_from(connection).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        connection,
        database: row.get(3)?,
        sql: row.get(4)?,
        interval_seconds: row.get(5)?,
        enabled: row.get(6)?,
        next_run: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            id: 0,
            name: "cleanup".into(),
            connection: 1,
            database: "main".into(),
            sql: "DELETE FROM t".into(),
            interval_seconds: 10,
            enabled: true,
            next_run: 0,
        }
    }

    #[test]
    fn due_jobs_are_claimed_once_and_missed_intervals_are_skipped() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let id = store.save(&job(), 100).unwrap();
        assert!(store.claim_due(109).unwrap().is_none());
        let (claimed, run) = store.claim_due(145).unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert!(store.claim_due(200).unwrap().is_none());
        assert!(store.delete(id).is_err());
        store.finish(run, 146, None).unwrap();
        assert_eq!(store.jobs().unwrap()[0].next_run, 155);
        assert_eq!(store.runs(id).unwrap()[0].status, "succeeded");
        assert!(store.claim_due(155).unwrap().is_some());
    }

    #[test]
    fn invalid_and_interrupted_jobs_are_visible() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let mut invalid = job();
        invalid.interval_seconds = 0;
        assert!(store.save(&invalid, 0).is_err());
        let id = store.save(&job(), 0).unwrap();
        store.claim_due(10).unwrap().unwrap();
        store.interrupt_abandoned(11).unwrap();
        assert_eq!(store.runs(id).unwrap()[0].status, "interrupted");
        assert!(store.claim_due(20).unwrap().is_some());
    }

    #[test]
    fn pause_stops_future_runs_and_long_runs_do_not_immediately_repeat() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let id = store.save(&job(), 0).unwrap();
        let (_, run) = store.claim_due(10).unwrap().unwrap();
        store.finish(run, 25, None).unwrap();
        assert!(store.claim_due(25).unwrap().is_none());
        let (_, run) = store.claim_due(35).unwrap().unwrap();
        store.pause(id).unwrap();
        store.finish(run, 36, None).unwrap();
        assert!(store.claim_due(100).unwrap().is_none());
        assert!(!store.jobs().unwrap()[0].enabled);
    }

    #[test]
    fn executes_sql_and_refuses_writes_on_read_only_connections() {
        use crate::db::{ConnectionConfig, Driver};
        let path = std::env::temp_dir().join(format!(
            "datacraft-scheduler-test-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE records(value INTEGER); INSERT INTO records VALUES (1)")
            .unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let mut task = job();
        task.sql = "UPDATE records SET value = value + 1".into();
        cfg.read_only = true;
        assert!(execute_with_config(&task, cfg.clone()).is_err());
        assert_eq!(
            db.query_row("SELECT value FROM records", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        cfg.read_only = false;
        execute_with_config(&task, cfg).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM records", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
