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
fn cli_volume_command_is_removed_and_never_touches_the_config() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let original = "[audio]\nvolume = 0.5\nchannels = 2\n";
    std::fs::write(&config, original).unwrap();

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "volume", "42"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no longer changes the speaker volume",
        ));

    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
}

#[test]
fn cli_volume_command_does_not_create_a_config_file() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args([
            "--config",
            config.to_str().unwrap(),
            "volume",
            "--whatever",
            "x",
        ])
        .assert()
        .failure();

    assert!(!config.exists());
}

#[test]
fn cli_help_does_not_advertise_the_removed_volume_command() {
    Command::cargo_bin("auto-airplay")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("volume").not());
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

#[test]
fn cli_config_show_json_warns_on_stderr_about_the_obsolete_volume() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(&config, "[audio]\nvolume = 0.5\nchannels = 2\n").unwrap();

    let assert = Command::cargo_bin("auto-airplay")
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
        .stderr(predicate::str::contains(
            "warning: obsolete configuration field `audio.volume` ignored",
        ));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(json["audio"].get("volume").is_none(), "{stdout}");
    assert_eq!(json["audio"]["channels"], 2);
}

#[test]
fn cli_warns_on_stderr_that_latency_ms_is_replaced_by_the_jitter_buffer() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(&config, "[audio]\nlatency_ms = 500\n").unwrap();

    let assert = Command::cargo_bin("auto-airplay")
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
        .stderr(predicate::str::contains(
            "warning: obsolete configuration field `audio.latency_ms` ignored: use `audio.jitter_buffer_ms` for input buffering",
        ));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(json["audio"].get("latency_ms").is_none(), "{stdout}");
    assert_eq!(json["audio"]["jitter_buffer_ms"], 400);
}

#[test]
fn cli_stays_quiet_without_obsolete_fields() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");

    Command::cargo_bin("auto-airplay")
        .unwrap()
        .args(["--config", config.to_str().unwrap(), "config", "show"])
        .assert()
        .success()
        .stderr(predicate::str::contains("warning").not());
}
