//! `kumo enable`/`kumo disable`: register (or remove) Kumo as a user-level background service that
//! starts automatically on login and restarts if it crashes, using each OS's native service
//! manager — a systemd user unit on Linux, a launchd user agent on macOS. Deliberately scoped to
//! the current user (no root/admin privileges needed) since Kumo is a personal, single-user tool,
//! not a system service shared across accounts.
//!
//! Windows has no equivalent here (see `kumo::daemon` for why `start`/`stop` still work there):
//! registering a proper auto-start-on-boot mechanism means either a Windows Service (which needs
//! an installer, admin rights, and a different process model entirely — `kumo run` was never
//! written to implement the Service Control Manager's start/stop protocol) or Task Scheduler XML.
//! Neither is worth the added complexity without a concrete need, so `enable`/`disable` on Windows
//! just explain that and point at `kumo start` instead.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[cfg(target_os = "linux")]
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("could not determine the kumo executable path")?;
    let unit_path = systemd_unit_path()?;
    let unit_dir = unit_path
        .parent()
        .context("systemd unit path has no parent directory")?;
    std::fs::create_dir_all(unit_dir)
        .with_context(|| format!("failed to create {}", unit_dir.display()))?;

    let unit = format!(
        "[Unit]\n\
         Description=Kumo personal agent gateway\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );
    std::fs::write(&unit_path, unit)
        .with_context(|| format!("failed to write {}", unit_path.display()))?;

    run_systemctl(&["--user", "daemon-reload"])?;
    run_systemctl(&["--user", "enable", "--now", "kumo.service"])?;

    println!("Kumo installed as a systemd user service and started.");
    println!("Unit file: {}", unit_path.display());
    println!("Logs: journalctl --user -u kumo.service -f");
    println!(
        "Note: user services normally only run while you are logged in. To have Kumo start \
         before login too, run: sudo loginctl enable-linger $USER"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn disable() -> Result<()> {
    let unit_path = systemd_unit_path()?;
    if !unit_path.exists() {
        println!("Kumo is not installed as a systemd service.");
        return Ok(());
    }

    // Best-effort: `disable --now` can fail harmlessly if the unit was already stopped or systemd
    // has no record of it (e.g. the unit file was deleted by hand); the unit file removal below is
    // what actually matters for "disable" to have taken effect.
    let _ = run_systemctl(&["--user", "disable", "--now", "kumo.service"]);
    std::fs::remove_file(&unit_path)
        .with_context(|| format!("failed to remove {}", unit_path.display()))?;
    let _ = run_systemctl(&["--user", "daemon-reload"]);

    println!("Kumo's systemd service was stopped and removed.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(".config/systemd/user/kumo.service"))
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()
        .context("failed to run systemctl (is systemd available?)")?;
    if !status.success() {
        bail!("systemctl {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("could not determine the kumo executable path")?;
    let plist_path = launchd_plist_path()?;
    let plist_dir = plist_path
        .parent()
        .context("launchd plist path has no parent directory")?;
    std::fs::create_dir_all(plist_dir)
        .with_context(|| format!("failed to create {}", plist_dir.display()))?;

    let log_path = crate::storage::data_dir()?.join("kumo.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log_path.display(),
    );
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("failed to write {}", plist_path.display()))?;

    run_launchctl(&["load", "-w", &plist_path.to_string_lossy()])?;

    println!("Kumo installed as a launchd agent and started.");
    println!("Plist: {}", plist_path.display());
    println!("Logs: {}", log_path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn disable() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        println!("Kumo is not installed as a launchd agent.");
        return Ok(());
    }

    let _ = run_launchctl(&["unload", "-w", &plist_path.to_string_lossy()]);
    std::fs::remove_file(&plist_path)
        .with_context(|| format!("failed to remove {}", plist_path.display()))?;

    println!("Kumo's launchd agent was stopped and removed.");
    Ok(())
}

#[cfg(target_os = "macos")]
const LABEL: &str = "com.kumo.agent";

#[cfg(target_os = "macos")]
fn launchd_plist_path() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("launchctl")
        .args(args)
        .status()
        .context("failed to run launchctl")?;
    if !status.success() {
        bail!("launchctl {} failed", args.join(" "));
    }
    Ok(())
}

#[cfg(windows)]
pub fn enable() -> Result<()> {
    println!(
        "kumo enable is not supported on Windows: registering a proper start-on-boot service \
         (a Windows Service, or a Task Scheduler task) is significantly more involved than the \
         systemd/launchd equivalents and isn't implemented yet."
    );
    println!("Use `kumo start` to run Kumo in the background for this login session instead.");
    Ok(())
}

#[cfg(windows)]
pub fn disable() -> Result<()> {
    println!("Kumo has no Windows auto-start service to disable (see `kumo enable`).");
    Ok(())
}
