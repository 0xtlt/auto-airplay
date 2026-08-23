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
