//! Durable tasks shared by the application and its background process.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::path::Path;
use std::time::Duration;

pub mod backup;
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
                    let (failed, message) = match std::panic::catch_unwind(|| execute(&job)) {
                        Ok(Ok(destination)) => (false, destination.unwrap_or_default()),
                        Ok(Err(error)) => (true, format!("{error:#}")),
                        Err(_) => (true, "Scheduled task stopped unexpectedly".into()),
                    };
                    loop {
                        match Store::open(&path)
                            .and_then(|mut store| store.complete(run_id, now(), failed, &message))
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

fn execute(job: &Job) -> anyhow::Result<Option<String>> {
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

fn execute_with_config(
    job: &Job,
    mut cfg: crate::db::ConnectionConfig,
) -> anyhow::Result<Option<String>> {
    use crate::db::{Driver, Session};

    if let Some(backup) = &job.backup {
        let database = if !job.database.is_empty() {
            job.database.as_str()
        } else if cfg.driver == Driver::Sqlite {
            "main"
        } else {
            cfg.database.as_str()
        };
        if cfg.driver == Driver::Oracle && database != cfg.database {
            bail!(crate::i18n::tr(
                "Oracle backup tasks must use the saved connection service"
            ));
        }
        if cfg.ask_password {
            bail!("Save the data source password before running a task in the background");
        }
        let request = backup.request(cfg.driver, database)?;
        let destination = match cfg.driver {
            Driver::Oracle => format!("{}/{}", request.directory, request.destination),
            Driver::Snowflake => format!(
                "{}.{}.{}",
                request.db, request.directory, request.destination
            ),
            _ => request.destination.clone(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(crate::db::backup::run(
            cfg,
            request,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        ))?;
        return Ok(Some(destination));
    }
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
        Ok(None)
    })
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub connection: u64,
    pub database: String,
    pub sql: String,
    pub backup: Option<backup::ScheduledBackup>,
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

#[cfg(unix)]
fn create_store_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            create_store_file(path)?;
        }
        let mut db = Connection::open(path)
            .with_context(|| format!("open schedule database {}", path.display()))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA foreign_keys = ON;")?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 connection INTEGER NOT NULL,
                 database_name TEXT NOT NULL,
                 sql_text TEXT NOT NULL,
                 interval_seconds INTEGER NOT NULL CHECK(interval_seconds > 0),
                 enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
                 next_run INTEGER NOT NULL,
                 backup_json TEXT
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
        let has_backup: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('jobs') WHERE name='backup_json')",
            [],
            |row| row.get(0),
        )?;
        if !has_backup {
            tx.execute("ALTER TABLE jobs ADD COLUMN backup_json TEXT", [])?;
        }
        tx.commit()?;
        Ok(Self(db))
    }

    pub fn jobs(&self) -> anyhow::Result<Vec<Job>> {
        let mut stmt = self.0.prepare("SELECT id, name, connection, database_name, sql_text, interval_seconds, enabled, next_run, backup_json FROM jobs ORDER BY name, id")?;
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
        let backup = job.backup.as_ref().map(serde_json::to_string).transpose()?;
        // Older daemons only understand SQL; a backup must fail harmlessly there.
        let sql = if backup.is_some() { "" } else { &job.sql };
        let next_run = now
            .checked_add(job.interval_seconds)
            .context("interval is too large")?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = if job.id == 0 {
            tx.execute("INSERT INTO jobs(name, connection, database_name, sql_text, interval_seconds, enabled, next_run, backup_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![job.name, connection, job.database, sql, job.interval_seconds, job.enabled, next_run, backup])?;
            tx.last_insert_rowid()
        } else {
            let changed = tx.execute("UPDATE jobs SET name=?1, connection=?2, database_name=?3, sql_text=?4, interval_seconds=?5, enabled=?6, next_run=?7, backup_json=?9 WHERE id=?8 AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id=?8 AND status='running')", params![job.name, connection, job.database, sql, job.interval_seconds, job.enabled, next_run, job.id, backup])?;
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
        let job = tx.query_row("SELECT id, name, connection, database_name, sql_text, interval_seconds, enabled, next_run, backup_json FROM jobs WHERE enabled=1 AND next_run<=?1 AND NOT EXISTS (SELECT 1 FROM runs WHERE job_id=jobs.id AND status='running') ORDER BY next_run, id LIMIT 1", [now], read_job).optional()?;
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
        self.complete(run_id, now, error.is_some(), error.unwrap_or(""))
    }

    fn complete(
        &mut self,
        run_id: i64,
        now: i64,
        failed: bool,
        message: &str,
    ) -> anyhow::Result<()> {
        let status = if failed { "failed" } else { "succeeded" };
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE runs SET finished=?1, status=?2, message=?3 WHERE id=?4 AND status='running'",
            params![now, status, message, run_id],
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
        || (job.backup.is_none() && job.sql.trim().is_empty())
        || job.interval_seconds <= 0
        || job.connection == 0
    {
        bail!("A name, data source, task and a positive interval are required");
    }
    i64::try_from(job.connection).context("connection ID is too large")?;
    now.checked_add(job.interval_seconds)
        .context("interval is too large")?;
    Ok(())
}

fn read_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let backup: Option<String> = row.get(8)?;
    let backup = backup
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
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
        backup,
        interval_seconds: row.get(5)?,
        enabled: row.get(6)?,
        next_run: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_oracle_target_must_match_the_saved_service_before_connecting() {
        let mut cfg = crate::db::ConnectionConfig::new(1, crate::db::Driver::Oracle);
        cfg.database = "XE".into();
        cfg.ask_password = true; // A bad implementation must stop without touching an external server.
        let mut task = job();
        task.database = "OTHER".into();
        task.backup = Some(backup::ScheduledBackup::new(crate::db::Driver::Oracle));
        let error = execute_with_config(&task, cfg.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved connection service"), "{error}");
        task.database = "XE".into();
        assert!(
            execute_with_config(&task, cfg)
                .unwrap_err()
                .to_string()
                .contains("Save the data source password")
        );
    }

    fn job() -> Job {
        Job {
            id: 0,
            name: "cleanup".into(),
            connection: 1,
            database: "main".into(),
            sql: "DELETE FROM t".into(),
            backup: None,
            interval_seconds: 10,
            enabled: true,
            next_run: 0,
        }
    }

    #[cfg(unix)]
    #[test]
    fn initializing_an_existing_store_file_preserves_active_sqlite_locks() {
        let root =
            std::env::temp_dir().join(format!("datacraft-scheduler-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("scheduler.sqlite");
        let store = Store::open(&path).unwrap();
        store.0.execute_batch("BEGIN").unwrap();
        store
            .0
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))
            .unwrap();

        // Probe the initialization boundary before a second SQLite connection can reacquire locks.
        create_store_file(&path).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scheduler::tests::sqlite_lock_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("DATACRAFT_TEST_SCHEDULER_LOCK_PROBE", &path)
            .output()
            .unwrap();
        store.0.execute_batch("ROLLBACK").unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "another process acquired an exclusive lock while a read transaction was active:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "internal child process lock probe"]
    fn sqlite_lock_probe() {
        let path = std::path::PathBuf::from(
            std::env::var_os("DATACRAFT_TEST_SCHEDULER_LOCK_PROBE").unwrap(),
        );
        assert!(path.is_file());
        assert!(
            path.canonicalize()
                .unwrap()
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        assert!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("datacraft-scheduler-lock-")
        );
        let connection = Connection::open(&path).unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        let result = connection.execute_batch("BEGIN EXCLUSIVE");
        if result.is_ok() {
            connection.execute_batch("ROLLBACK").unwrap();
        }
        assert!(
            matches!(
                &result,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if matches!(
                        error.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    )
            ),
            "expected the other process's read lock to prevent BEGIN EXCLUSIVE, got {result:?}"
        );
    }

    #[test]
    fn legacy_jobs_and_history_migrate_without_changes() {
        let path = std::env::temp_dir().join(format!(
            "datacraft-scheduler-migration-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch(
            "CREATE TABLE jobs(id INTEGER PRIMARY KEY, name TEXT NOT NULL, connection INTEGER NOT NULL, database_name TEXT NOT NULL, sql_text TEXT NOT NULL, interval_seconds INTEGER NOT NULL, enabled INTEGER NOT NULL, next_run INTEGER NOT NULL);
             CREATE TABLE runs(id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL REFERENCES jobs(id), started INTEGER NOT NULL, finished INTEGER, status TEXT NOT NULL, message TEXT NOT NULL DEFAULT '');
             INSERT INTO jobs VALUES(7, 'legacy SQL', 42, 'shop', 'DELETE FROM expired', 60, 1, 100);
             INSERT INTO runs VALUES(9, 7, 1, 2, 'failed', 'original error');",
        ).unwrap();
        drop(legacy);
        {
            let store = Store::open(&path).unwrap();
            let jobs = store.jobs().unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].id, 7);
            assert_eq!(jobs[0].connection, 42);
            assert_eq!(jobs[0].database, "shop");
            assert_eq!(jobs[0].sql, "DELETE FROM expired");
            assert_eq!(jobs[0].next_run, 100);
            let runs = store.runs(7).unwrap();
            assert_eq!(runs[0].id, 9);
            assert_eq!(runs[0].message, "original error");
            let payload: Option<String> = store
                .0
                .query_row("SELECT backup_json FROM jobs WHERE id=7", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(payload, None);
        }
        assert_eq!(Store::open(&path).unwrap().jobs().unwrap().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn backup_jobs_round_trip_without_sql_and_corrupt_payloads_are_reported() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let mut task = job();
        task.sql.clear();
        task.backup = Some(backup::ScheduledBackup::new(crate::db::Driver::Sqlite));
        let id = store.save(&task, 0).unwrap();
        assert_eq!(store.jobs().unwrap()[0].backup, task.backup);
        task.id = id;
        task.sql = "DELETE FROM important_data".into();
        store.save(&task, 0).unwrap();
        let sql: String = store
            .0
            .query_row("SELECT sql_text FROM jobs WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            sql.is_empty(),
            "old daemons must never execute stale SQL from backup jobs"
        );
        let (claimed, run) = store.claim_due(10).unwrap().unwrap();
        assert_eq!(claimed.backup, task.backup);
        store
            .complete(run, 11, false, "backup destination")
            .unwrap();
        let runs = store.runs(id).unwrap();
        assert_eq!(runs[0].status, "succeeded");
        assert_eq!(runs[0].message, "backup destination");
        store
            .0
            .execute(
                "UPDATE jobs SET backup_json='not valid JSON' WHERE id=?1",
                [id],
            )
            .unwrap();
        assert!(store.jobs().is_err());
        assert!(store.claim_due(20).is_err());
    }

    #[test]
    fn repeated_sqlite_backups_preserve_snapshots_on_read_only_sources() {
        use crate::db::{ConnectionConfig, Driver};
        let root = std::env::temp_dir().join(format!(
            "datacraft-scheduler-backup-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("source.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE records(value INTEGER); INSERT INTO records VALUES(1)")
            .unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        cfg.read_only = true;
        let mut task = job();
        let mut backup = backup::ScheduledBackup::new(Driver::Sqlite);
        backup.folder = root.to_string_lossy().into_owned();
        task.backup = Some(backup.clone());
        // Empty database names fall back to SQLite's main database, never stale SQL.
        task.database.clear();
        task.sql = "DROP TABLE records".into();
        let first = execute_with_config(&task, cfg.clone()).unwrap().unwrap();
        db.execute("UPDATE records SET value=2", []).unwrap();
        let second = execute_with_config(&task, cfg.clone()).unwrap().unwrap();
        assert_ne!(first, second);
        for (destination, value) in [(&first, 1), (&second, 2)] {
            let snapshot = Connection::open(destination).unwrap();
            assert_eq!(
                snapshot
                    .query_row("SELECT value FROM records", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                value
            );
        }
        assert_eq!(
            db.query_row("SELECT value FROM records", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        backup.folder = root.join("missing").to_string_lossy().into_owned();
        task.backup = Some(backup);
        assert!(execute_with_config(&task, cfg.clone()).is_err());
        assert!(Path::new(&first).is_file() && Path::new(&second).is_file());
        task.database = "attached".into();
        assert!(execute_with_config(&task, cfg).is_err());
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scheduled_server_backups_refuse_read_only_sources_before_connecting() {
        use crate::db::{ConnectionConfig, Driver};
        let mut task = job();
        task.database = "shop".into();
        let mut backup = backup::ScheduledBackup::new(Driver::MsSql);
        backup.folder = "/server/backups".into();
        task.backup = Some(backup);
        let mut cfg = ConnectionConfig::new(1, Driver::MsSql);
        cfg.read_only = true;
        let error = execute_with_config(&task, cfg).unwrap_err();
        assert!(format!("{error:#}").contains("read-only"));
        task.database.clear();
        let mut cfg = ConnectionConfig::new(1, Driver::MsSql);
        cfg.database = "saved_database".into();
        cfg.read_only = true;
        let error = execute_with_config(&task, cfg).unwrap_err();
        assert!(format!("{error:#}").contains("read-only"));
    }

    #[test]
    fn scheduled_backups_require_a_resolved_database_and_saved_credentials() {
        use crate::db::{ConnectionConfig, Driver};
        let mut task = job();
        task.database.clear();
        task.backup = Some(backup::ScheduledBackup::new(Driver::Oracle));
        let mut cfg = ConnectionConfig::new(1, Driver::Oracle);
        cfg.database.clear();
        let error = execute_with_config(&task, cfg).unwrap_err();
        assert!(format!("{error:#}").contains("Choose a database"));
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.ask_password = true;
        assert!(format!("{:#}", execute_with_config(&task, cfg).unwrap_err()).contains("password"));
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
