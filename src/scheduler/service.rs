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
        let plist = launchd_plist("/Applications/A & B <app>/DataCraft").unwrap();
        assert!(plist.contains("<string>/Applications/A &amp; B &lt;app&gt;/DataCraft</string>"));
        assert!(launchd_plist("/bad\0path").is_err());
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
