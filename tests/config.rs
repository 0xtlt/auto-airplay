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

#[test]
fn legacy_audio_volume_is_accepted_and_reported_as_obsolete() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "[output]\nselector = \"Living Room\"\n\n[audio]\nvolume = 0.5\nchannels = 2\n",
    )
    .unwrap();

    let config = ConfigStore::new(path).load().unwrap();

    assert_eq!(config.obsolete_fields(), ["audio.volume"]);
    assert_eq!(config.output.selector.as_deref(), Some("Living Room"));
    assert_eq!(config.audio.channels, 2);
}

#[test]
fn legacy_audio_volume_accepts_any_value_type() {
    for value in ["\"loud\"", "42", "true", "[1, 2]", "{ left = 1 }"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!("[audio]\nvolume = {value}\n")).unwrap();

        let config = ConfigStore::new(path).load().unwrap();

        assert_eq!(config.obsolete_fields(), ["audio.volume"], "value {value}");
    }
}

#[test]
fn configuration_without_legacy_volume_reports_no_obsolete_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[audio]\nchannels = 2\n").unwrap();

    let config = ConfigStore::new(path).load().unwrap();

    assert!(config.obsolete_fields().is_empty());
    assert!(AppConfig::default().obsolete_fields().is_empty());
}

#[test]
fn saving_a_legacy_configuration_drops_the_volume_key() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[audio]\nvolume = 0.5\nlatency_ms = 321\n").unwrap();
    let store = ConfigStore::new(path.clone());

    let config = store.load().unwrap();
    store.save(&config).unwrap();

    let raw = std::fs::read_to_string(path).unwrap();
    assert!(!raw.contains("volume"), "{raw}");
    assert!(raw.contains("latency_ms = 321"));
    assert!(store.load().unwrap().obsolete_fields().is_empty());
}

#[test]
fn default_configuration_has_no_volume_key() {
    let raw = toml::to_string_pretty(&AppConfig::default()).unwrap();

    assert!(!raw.contains("volume"), "{raw}");
}

#[test]
fn editing_the_obsolete_volume_value_does_not_change_the_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let store = ConfigStore::new(path.clone());

    std::fs::write(&path, "[audio]\nvolume = 0.2\n").unwrap();
    let before = store.load().unwrap();
    std::fs::write(&path, "[audio]\nvolume = 0.9\n").unwrap();

    assert_eq!(store.load().unwrap(), before);
}

#[test]
fn unknown_audio_fields_are_still_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[audio]\nloudness = 0.5\n").unwrap();

    assert!(ConfigStore::new(path).load().is_err());
}

fn audio_from(toml: &str) -> auto_airplay::config::AudioConfig {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, toml).unwrap();
    ConfigStore::new(path).load().unwrap().audio
}

#[test]
fn comparison_ignores_the_obsolete_volume_field_entirely() {
    let without = audio_from("[audio]\nlatency_ms = 321\n");
    let with = audio_from("[audio]\nvolume = 0.5\nlatency_ms = 321\n");
    let changed = audio_from("[audio]\nvolume = 0.9\nlatency_ms = 321\n");

    assert_eq!(without, with, "adding the field");
    assert_eq!(with, without, "removing the field");
    assert_eq!(with, changed, "changing the field");

    let whole = |audio| AppConfig {
        audio,
        ..AppConfig::default()
    };
    assert_eq!(whole(with), whole(without));
}

#[test]
fn comparison_still_detects_real_audio_changes() {
    let base = audio_from("[audio]\nvolume = 0.5\nlatency_ms = 321\n");

    assert_ne!(
        base,
        audio_from("[audio]\nvolume = 0.5\nlatency_ms = 322\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\nlatency_ms = 321\nchannels = 1\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\nlatency_ms = 321\npreferred_sample_rate = 44100\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\nlatency_ms = 321\nsilence_threshold_dbfs = -50.0\n")
    );
}
