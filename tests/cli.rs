use std::{fs, process::Command};

fn kumo() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kumo"))
}

#[test]
fn help_and_version_are_available_without_configuration() {
    let help = kumo().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Kumo personal agent gateway"));

    let version = kumo().arg("--version").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("kumo {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_cli_input_fails_with_usage_guidance() {
    let output = kumo().arg("unknown").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown command 'unknown'"));
    assert!(stderr.contains("kumo --help"));
}

/// Unix only, because the isolation this test depends on cannot be arranged on Windows: the
/// `directories` crate resolves the config directory through the Known Folder API rather than the
/// environment, so `HOME` and `XDG_CONFIG_HOME` do not move it and `status` finds the real
/// `%APPDATA%\kumo\kumo.toml` of whoever is running the suite. Giving Windows this coverage would
/// mean an explicit override for the config directory, the way `KUMO_DATA_DIR` already overrides
/// the data one — a deliberate addition, not something to slip in for a test.
#[cfg(unix)]
#[test]
fn status_reports_an_unconfigured_isolated_home() {
    let home = std::env::temp_dir().join(format!("kumo-cli-{}", std::process::id()));
    fs::create_dir_all(&home).unwrap();
    let output = kumo()
        .arg("status")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Config:    not set up yet"));
    let _ = fs::remove_dir_all(home);
}
