use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn cli_configures_input_and_output_without_editing_toml() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "input",
            "set-bluetooth",
            "AA:BB:CC:DD:EE:FF",
            "--name",
            "My Turntable",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("My Turntable"));

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "output",
            "set",
            "Living Room",
            "--protocol",
            "airplay1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Living Room"));

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "config",
            "show",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("AA:BB:CC:DD:EE:FF"))
        .stdout(predicate::str::contains("\"selector\": \"Living Room\""));
}

#[test]
fn cli_rejects_invalid_bluetooth_addresses_without_writing_them() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "input",
            "set-bluetooth",
            "not-a-mac",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid Bluetooth address"));

    assert!(!config.exists());
}

#[test]
fn cli_updates_volume_as_a_percentage() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "volume", "42"])
        .assert()
        .success()
        .stdout(predicate::str::contains("42%"));

    let raw = std::fs::read_to_string(config).unwrap();
    assert!(raw.contains("volume = 0.42"));
}

#[test]
fn cli_defaults_to_airplay2_for_live_streaming() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "output",
            "set",
            "Living Room",
        ])
        .assert()
        .success();

    let raw = std::fs::read_to_string(config).unwrap();
    assert!(raw.contains("protocol = \"air_play2\""));
}
