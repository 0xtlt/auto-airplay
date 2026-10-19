use auto_airplay::config::{AppConfig, ConfigStore, OutputProtocol};

#[test]
fn configuration_round_trips_without_losing_device_choices() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let store = ConfigStore::new(path);

    let mut expected = AppConfig::default();
    expected.input.bluetooth_address = Some("AA:BB:CC:DD:EE:FF".into());
    expected.input.display_name = Some("My Turntable".into());
    expected.output.selector = Some("Living Room".into());
    expected.output.protocol = OutputProtocol::AirPlay1;

    store.save(&expected).unwrap();

    assert_eq!(store.load().unwrap(), expected);
}

#[test]
fn configuration_is_created_with_safe_non_secret_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/config.toml");
    let store = ConfigStore::new(path.clone());

    let config = store.load_or_create().unwrap();

    assert_eq!(config, AppConfig::default());
    let serialized = std::fs::read_to_string(path).unwrap();
    assert!(!serialized.to_ascii_lowercase().contains("password"));
    assert!(!serialized.to_ascii_lowercase().contains("pin"));
}

#[test]
fn silence_threshold_defaults_when_missing_from_an_existing_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "[audio]\nvolume = 0.5\nchannels = 2\nlatency_ms = 500\n",
    )
    .unwrap();

    let config = ConfigStore::new(path).load().unwrap();

    assert_eq!(config.audio.volume, 0.5);
    assert_eq!(config.audio.silence_threshold_dbfs, -40.0);
    assert_eq!(
        config.audio.silence_threshold_dbfs,
        AppConfig::default().audio.silence_threshold_dbfs
    );
}

#[test]
fn silence_threshold_can_be_configured() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[audio]\nsilence_threshold_dbfs = -42.5\n").unwrap();

    let config = ConfigStore::new(path).load().unwrap();

    assert_eq!(config.audio.silence_threshold_dbfs, -42.5);
}

#[test]
fn silence_threshold_survives_a_save_and_load() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(directory.path().join("config.toml"));
    let mut expected = AppConfig::default();
    expected.audio.silence_threshold_dbfs = -60.0;

    store.save(&expected).unwrap();

    assert_eq!(store.load().unwrap().audio.silence_threshold_dbfs, -60.0);
}
