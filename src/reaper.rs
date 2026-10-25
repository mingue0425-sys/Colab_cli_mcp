//! Idle auto-stop. A detached `colab reaper` process checks once a minute and
//! stops runtimes colab-cli manages once they have been idle for the configured
//! minutes. One reaper runs at a time (file lock); it exits when it has nothing
//! left to watch.

use crate::{config, error::Result, ops::App, runtime};
use std::{io::Write, os::unix::process::CommandExt, path::Path, time::Duration};

const CHECK_EVERY: Duration = Duration::from_secs(60);

fn lock_file(home: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("reaper.lock"))
}

/// Start the reaper unless one is already running or auto-stop is disabled.
pub fn ensure_running(home: &Path) {
    if runtime::idle_stop_minutes(home) == 0 {
        return;
    }
    let Ok(file) = lock_file(home) else { return };
    if file.try_lock().is_err() {
        return; // A reaper holds the lock.
    }
    drop(file);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("reaper")
        .env("COLAB_CLI_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Own process group: survives the terminal or agent that started it.
        .process_group(0)
        .spawn();
}

fn log(home: &Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("reaper.log"))
    {
        let _ = writeln!(file, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

pub async fn run(app: App) -> Result<()> {
    let file = lock_file(&app.home)?;
    if file.try_lock().is_err() {
        return Ok(());
    }
    config::private_dir(&app.home)?;
    let mut quiet_rounds = 0;
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if runtime::idle_stop_minutes(&app.home) == 0 {
            break;
        }
        let watching = match app.reap_once().await {
            Ok((stopped, watching)) => {
                for endpoint in &stopped {
                    log(&app.home, &format!("stopped idle runtime {endpoint}"));
                }
                watching
            }
            Err(error) => {
                log(&app.home, &format!("check failed: {}", error.code));
                1
            }
        };
        // Exit after ~3 minutes with nothing to watch; the next run restarts it.
        quiet_rounds = if watching == 0 { quiet_rounds + 1 } else { 0 };
        if quiet_rounds >= 3 {
            break;
        }
    }
    drop(file);
    Ok(())
}
