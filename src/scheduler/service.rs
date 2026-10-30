//! Per-user startup registration for the scheduler process.

use anyhow::{Context, bail};
use std::path::Path;
use std::process::Command;

fn command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .output()
        .with_context(|| format!("start {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn enable() -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("find the DataCraft executable")?;
    install(&executable)?;
    wait_for_start()
}

fn wait_for_start() -> anyhow::Result<()> {
    for _ in 0..50 {
        if is_running()? {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("The background scheduler did not start; check the system startup settings")
}

pub(super) fn is_running() -> anyhow::Result<bool> {
    let path = super::database_path()?.with_extension("lock");
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("open scheduler lock"),
    };
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("check scheduler lock"),
    }
}

#[cfg(windows)]
fn install(executable: &Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    let executable = executable
        .to_str()
        .context("Executable path is not valid Unicode")?;
    register_windows(
        executable,
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
    )?;
    Command::new(executable)
        .arg("--scheduler")
        .creation_flags(0x08000000)
        .spawn()
        .context("start background scheduler")?;
    Ok(())
}

#[cfg(windows)]
fn register_windows(executable: &str, key: &str) -> anyhow::Result<()> {
    let value = format!("\"{executable}\" --scheduler");
    command(
        "reg",
        &[
            "add",
            key,
            "/v",
            "DataCraftScheduler",
            "/t",
            "REG_SZ",
            "/d",
            &value,
            "/f",
        ],
    )
}

#[cfg(target_os = "linux")]
fn install(executable: &Path) -> anyhow::Result<()> {
    if command("systemctl", &["--user", "show-environment"]).is_err() {
        return install_xdg(executable);
    }
    let dir = dirs::config_dir()
        .context("find user configuration directory")?
        .join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit = dir.join("datacraft-scheduler.service");
    let path = executable
        .to_str()
        .context("Executable path is not valid Unicode")?;
    std::fs::write(unit, systemd_unit(path)?)?;
    command("systemctl", &["--user", "daemon-reload"])?;
    command(
        "systemctl",
        &["--user", "enable", "--now", "datacraft-scheduler.service"],
    )
}

#[cfg(target_os = "linux")]
fn install_xdg(executable: &Path) -> anyhow::Result<()> {
    let path = executable
        .to_str()
        .context("Executable path is not valid Unicode")?;
    let dir = dirs::config_dir()
        .context("find user configuration directory")?
        .join("autostart");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("datacraft-scheduler.desktop"), xdg_entry(path)?)?;
    let mut child = Command::new(executable)
        .arg("--scheduler")
        .spawn()
        .context("start background scheduler")?;
    std::thread::Builder::new()
        .name("sql-scheduler-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install(executable: &Path) -> anyhow::Result<()> {
    let dir = dirs::home_dir()
        .context("find home directory")?
        .join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let path = executable
        .to_str()
        .context("Executable path is not valid Unicode")?;
    let file = dir.join("com.datacraft.scheduler.plist");
    std::fs::write(&file, launchd_plist(path)?)?;
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .context("get user ID")?;
    if !uid.status.success() {
        bail!("Could not determine user ID");
    }
    let domain = format!("gui/{}", String::from_utf8(uid.stdout)?.trim());
    let label = format!("{domain}/com.datacraft.scheduler");
    if Command::new("launchctl")
        .args(["print", &label])
        .output()
        .is_ok_and(|out| out.status.success())
    {
        // Do not stop a running SQL statement merely to update the launch path.
        return command("launchctl", &["kickstart", &label]);
    }
    command(
        "launchctl",
        &[
            "bootstrap",
            &domain,
            file.to_str()
                .context("LaunchAgent path is not valid Unicode")?,
        ],
    )
}

#[cfg(any(test, target_os = "linux"))]
fn systemd_unit(path: &str) -> anyhow::Result<String> {
    if path.chars().any(char::is_control) {
        bail!("Executable path contains control characters");
    }
    let path = path
        .replace('%', "%%")
        .replace('$', "$$")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    Ok(format!(
        "[Unit]\nDescription=DataCraft SQL scheduler\n\n[Service]\nType=exec\nExecStart=\"{path}\" --scheduler\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[cfg(any(test, target_os = "linux"))]
fn xdg_entry(path: &str) -> anyhow::Result<String> {
    if path.chars().any(char::is_control) || path.contains(['=', '%']) {
        bail!("Executable path cannot be represented in XDG autostart");
    }
    let mut escaped = String::new();
    for ch in path.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\\\\\"),
            '"' => escaped.push_str("\\\\\""),
            '$' => escaped.push_str("\\\\$"),
            '`' => escaped.push_str("\\\\`"),
            ch => escaped.push(ch),
        }
    }
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=DataCraft SQL scheduler\nExec=\"{escaped}\" --scheduler\nTerminal=false\n"
    ))
}

#[cfg(any(test, target_os = "macos"))]
fn launchd_plist(path: &str) -> anyhow::Result<String> {
    if path
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        bail!("Executable path contains invalid XML characters");
    }
    let escaped = path
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.datacraft.scheduler</string><key>ProgramArguments</key><array><string>{escaped}</string><string>--scheduler</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_files_preserve_special_path_characters() {
        let unit = systemd_unit("/home/a b/$name%1/\\\"datacraft").unwrap();
        assert!(unit.contains("ExecStart=\"/home/a b/$$name%%1/\\\\\\\"datacraft\" --scheduler"));
        assert!(systemd_unit("/bad\npath").is_err());
        let entry = xdg_entry("/home/a b/$name/\\\"data").unwrap();
        assert!(entry.contains("Exec=\"/home/a b/\\\\$name/\\\\\\\\\\\\\"data\" --scheduler"));
        assert!(xdg_entry("/bad%name").is_err());
        let plist = launchd_plist("/Applications/A & B <app>/DataCraft").unwrap();
        assert!(plist.contains("<string>/Applications/A &amp; B &lt;app&gt;/DataCraft</string>"));
        assert!(launchd_plist("/bad\0path").is_err());
    }

    /// Runs only in an isolated GitHub-hosted runner with its user service manager configured.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "requires a disposable CI user session and its native service manager"]
    fn native_service_runs_sql_without_the_gui() {
        use crate::db::{AuthMode, ConnectionConfig, Driver};
        use crate::scheduler::{Job, Store, now};

        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        let profile =
            std::path::PathBuf::from(std::env::var_os("DATACRAFT_TEST_DATA_DIR").unwrap());
        let runner_temp = std::path::PathBuf::from(std::env::var_os("RUNNER_TEMP").unwrap());
        std::fs::create_dir_all(&profile).unwrap();
        assert!(
            profile
                .canonicalize()
                .unwrap()
                .starts_with(runner_temp.canonicalize().unwrap())
        );

        #[cfg(target_os = "linux")]
        let xdg = std::env::var_os("DATACRAFT_TEST_XDG").is_some();
        struct Cleanup {
            #[cfg(target_os = "linux")]
            xdg: bool,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                #[cfg(target_os = "linux")]
                {
                    if let Some(dir) = dirs::config_dir() {
                        if self.xdg {
                            let _ = command("pkill", &["-x", "datacraft"]);
                            let _ = std::fs::remove_file(
                                dir.join("autostart/datacraft-scheduler.desktop"),
                            );
                        } else {
                            let _ = command(
                                "systemctl",
                                &["--user", "disable", "--now", "datacraft-scheduler.service"],
                            );
                            let _ = std::fs::remove_file(
                                dir.join("systemd/user/datacraft-scheduler.service"),
                            );
                            let _ = command("systemctl", &["--user", "daemon-reload"]);
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    if let Ok(uid) = Command::new("id").arg("-u").output() {
                        let label = format!(
                            "gui/{}/com.datacraft.scheduler",
                            String::from_utf8_lossy(&uid.stdout).trim()
                        );
                        let _ = command("launchctl", &["bootout", &label]);
                    }
                    if let Some(dir) = dirs::home_dir() {
                        let _ = std::fs::remove_file(
                            dir.join("Library/LaunchAgents/com.datacraft.scheduler.plist"),
                        );
                    }
                }
            }
        }
        let _cleanup = Cleanup {
            #[cfg(target_os = "linux")]
            xdg,
        };
        let database = profile.join("sample.sqlite");
        let sample = rusqlite::Connection::open(&database).unwrap();
        sample
            .execute_batch("CREATE TABLE probe(value INTEGER); INSERT INTO probe VALUES(0);")
            .unwrap();
        let mut cfg = ConnectionConfig::new(1, Driver::Sqlite);
        cfg.auth = AuthMode::NoAuth;
        cfg.file_path = database.to_string_lossy().into_owned();
        std::fs::write(
            profile.join("connections.json"),
            serde_json::to_string(&vec![cfg]).unwrap(),
        )
        .unwrap();
        let mut store = Store::open(&profile.join("scheduler.sqlite")).unwrap();
        let job = Job {
            id: 0,
            name: "native service test".into(),
            connection: 1,
            database: "main".into(),
            sql: "UPDATE probe SET value = value + 1".into(),
            interval_seconds: 30,
            enabled: true,
            next_run: 0,
        };
        let id = store.save(&job, now() - 30).unwrap();
        let executable =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/datacraft");
        #[cfg(target_os = "linux")]
        if xdg {
            install_xdg(&executable).unwrap();
            let desktop = dirs::config_dir()
                .unwrap()
                .join("autostart/datacraft-scheduler.desktop");
            assert!(
                Command::new("desktop-file-validate")
                    .arg(desktop)
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            install(&executable).unwrap();
        }
        #[cfg(target_os = "macos")]
        install(&executable).unwrap();
        wait_for_start().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let runs = store.runs(id).unwrap();
            if let Some(run) = runs.first().filter(|run| run.status != "running") {
                assert_eq!(run.status, "succeeded", "{}", run.message);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "service did not finish the task"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(
            sample
                .query_row("SELECT value FROM probe", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_autostart_value_quotes_the_executable_path() {
        struct TestKey(String);
        impl Drop for TestKey {
            fn drop(&mut self) {
                let _ = command("reg", &["delete", &self.0, "/f"]);
            }
        }
        let key = TestKey(format!(
            r"HKCU\Software\DataCraftSchedulerTests\{}",
            uuid::Uuid::new_v4()
        ));
        register_windows(r"C:\Program Files\DataCraft\DataCraft.exe", &key.0).unwrap();
        let result = Command::new("reg")
            .args(["query", &key.0, "/v", "DataCraftScheduler"])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stdout)
                .contains(r#""C:\Program Files\DataCraft\DataCraft.exe" --scheduler"#)
        );
    }
}
