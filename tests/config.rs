use auto_airplay::config::{
    AppConfig, ConfigStore, DEFAULT_SPEAKER_BUFFER_MS, OutputProtocol, SPEAKER_BUFFER_MS_RANGE,
    StreamMode,
};

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
    std::fs::write(
        &path,
        "[audio]\nvolume = 0.5\nlatency_ms = 321\njitter_buffer_ms = 750\n",
    )
    .unwrap();
    let store = ConfigStore::new(path.clone());

    let config = store.load().unwrap();
    store.save(&config).unwrap();

    let raw = std::fs::read_to_string(path).unwrap();
    assert!(!raw.contains("volume"), "{raw}");
    assert!(!raw.contains("latency_ms"), "{raw}");
    assert!(raw.contains("jitter_buffer_ms = 750"), "{raw}");
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
    let without = audio_from("[audio]\njitter_buffer_ms = 321\n");
    let with = audio_from("[audio]\nvolume = 0.5\njitter_buffer_ms = 321\n");
    let changed = audio_from("[audio]\nvolume = 0.9\njitter_buffer_ms = 321\n");

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
    let base = audio_from("[audio]\nvolume = 0.5\njitter_buffer_ms = 321\n");

    assert_ne!(
        base,
        audio_from("[audio]\nvolume = 0.5\njitter_buffer_ms = 322\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\njitter_buffer_ms = 321\nchannels = 1\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\njitter_buffer_ms = 321\npreferred_sample_rate = 44100\n")
    );
    assert_ne!(
        base,
        audio_from("[audio]\njitter_buffer_ms = 321\nsilence_threshold_dbfs = -50.0\n")
    );
}

fn config_from(raw: &str) -> AppConfig {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, raw).unwrap();
    ConfigStore::new(path).load().unwrap()
}

#[test]
fn sound_detection_defaults_apply_to_configs_without_the_new_keys() {
    // The production configuration: only the old silence key is set.
    let config = config_from(
        "[audio]\nsilence_threshold_dbfs = -40\n\n[runtime]\nidle_silence_seconds = 30\n",
    );

    assert_eq!(config.audio.sound_threshold_dbfs, None);
    assert_eq!(config.audio.effective_sound_threshold_dbfs(), -34.0);
    assert_eq!(config.runtime.sound_confirm_ms, 2_000);
    assert_eq!(config.runtime.preempted_silence_seconds, 5);
    assert_eq!(config.runtime.preemption_backoff_seconds, 30);
    assert_eq!(config.runtime.preemption_backoff_max_seconds, 600);
    assert!(config.validate_detection().is_ok());
    assert_eq!(config_from(""), AppConfig::default());
}

#[test]
fn default_sound_threshold_follows_the_silence_threshold() {
    let mut config = AppConfig::default();
    config.audio.silence_threshold_dbfs = -50.0;
    assert_eq!(config.audio.effective_sound_threshold_dbfs(), -44.0);

    // Never above full scale, which no level can exceed.
    config.audio.silence_threshold_dbfs = -2.0;
    assert_eq!(config.audio.effective_sound_threshold_dbfs(), 0.0);
}

#[test]
fn sound_detection_keys_can_be_configured_and_survive_a_save() {
    let config = config_from(
        "[audio]\nsilence_threshold_dbfs = -45.0\nsound_threshold_dbfs = -30.5\n\n[runtime]\nsound_confirm_ms = 3500\n",
    );
    assert_eq!(config.audio.effective_sound_threshold_dbfs(), -30.5);
    assert_eq!(config.runtime.sound_confirm_ms, 3_500);

    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(directory.path().join("config.toml"));
    store.save(&config).unwrap();
    assert_eq!(store.load().unwrap(), config);
}

#[test]
fn changing_the_sound_threshold_counts_as_a_configuration_change() {
    let base = AppConfig::default();
    let mut changed = base.clone();
    changed.audio.sound_threshold_dbfs = Some(-34.0);
    assert_ne!(base, changed);

    let mut changed = base.clone();
    changed.runtime.sound_confirm_ms = 3_000;
    assert_ne!(base, changed);
}

#[test]
fn sound_detection_validation_rejects_bad_values() {
    let error = |edit: &dyn Fn(&mut AppConfig)| {
        let mut config = AppConfig::default();
        edit(&mut config);
        config.validate_detection().unwrap_err()
    };

    assert!(error(&|c| c.audio.silence_threshold_dbfs = f32::NAN).contains("silence_threshold"));
    assert!(error(&|c| c.audio.silence_threshold_dbfs = -121.0).contains("silence_threshold"));
    assert!(error(&|c| c.audio.silence_threshold_dbfs = 1.0).contains("silence_threshold"));
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -121.0, 0.5] {
        assert!(
            error(&|c| c.audio.sound_threshold_dbfs = Some(bad)).contains("between -120.0 and 0.0")
        );
    }
    // Below the silence threshold (-40 by default).
    assert!(error(&|c| c.audio.sound_threshold_dbfs = Some(-41.0)).contains("must not be below"));
    for bad in [0, 249, 60_001] {
        assert!(error(&|c| c.runtime.sound_confirm_ms = bad).contains("sound_confirm_ms"));
    }
}

#[test]
fn sound_detection_validation_accepts_the_edges() {
    let mut config = AppConfig::default();
    config.audio.sound_threshold_dbfs = Some(-40.0);
    config.runtime.sound_confirm_ms = 250;
    assert!(config.validate_detection().is_ok());

    config.audio.silence_threshold_dbfs = -120.0;
    config.audio.sound_threshold_dbfs = Some(0.0);
    config.runtime.sound_confirm_ms = 60_000;
    assert!(config.validate_detection().is_ok());
}

#[test]
fn preemption_keys_can_be_configured_and_survive_a_save() {
    let config = config_from(
        "[runtime]\npreempted_silence_seconds = 10\npreemption_backoff_seconds = 60\npreemption_backoff_max_seconds = 900\n",
    );
    assert_eq!(config.runtime.preempted_silence_seconds, 10);
    assert_eq!(config.runtime.preemption_backoff_seconds, 60);
    assert_eq!(config.runtime.preemption_backoff_max_seconds, 900);
    assert!(config.validate_detection().is_ok());

    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(directory.path().join("config.toml"));
    store.save(&config).unwrap();
    assert_eq!(store.load().unwrap(), config);
}

#[test]
fn preemption_validation_rejects_bad_values() {
    let error = |edit: &dyn Fn(&mut AppConfig)| {
        let mut config = AppConfig::default();
        edit(&mut config);
        config.validate_detection().unwrap_err()
    };

    for bad in [0, 3_601] {
        assert!(
            error(&|c| c.runtime.preempted_silence_seconds = bad)
                .contains("preempted_silence_seconds")
        );
    }
    assert!(
        error(&|c| c.runtime.preemption_backoff_seconds = 0)
            .contains("preemption_backoff_seconds must be at least 1")
    );
    // The maximum may not be below the base (30 by default).
    assert!(
        error(&|c| c.runtime.preemption_backoff_max_seconds = 29)
            .contains("must not be below preemption_backoff_seconds")
    );
    assert!(
        error(&|c| c.runtime.preemption_backoff_max_seconds = 86_401).contains("at most 86400")
    );
}

#[test]
fn preemption_validation_accepts_the_edges() {
    let mut config = AppConfig::default();
    config.runtime.preempted_silence_seconds = 1;
    config.runtime.preemption_backoff_seconds = 1;
    config.runtime.preemption_backoff_max_seconds = 1;
    assert!(config.validate_detection().is_ok());

    config.runtime.preempted_silence_seconds = 3_600;
    config.runtime.preemption_backoff_seconds = 86_400;
    config.runtime.preemption_backoff_max_seconds = 86_400;
    assert!(config.validate_detection().is_ok());
}

#[test]
fn changing_a_preemption_key_counts_as_a_configuration_change() {
    let base = AppConfig::default();
    for edit in [
        (|c: &mut AppConfig| c.runtime.preempted_silence_seconds = 6) as fn(&mut AppConfig),
        |c| c.runtime.preemption_backoff_seconds = 31,
        |c| c.runtime.preemption_backoff_max_seconds = 601,
    ] {
        let mut changed = base.clone();
        edit(&mut changed);
        assert_ne!(base, changed);
    }
}

#[test]
fn legacy_latency_ms_is_accepted_ignored_reported_and_dropped_on_save() {
    for value in ["500", "0", "99999", "\"soon\"", "[1, 2]"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!("[audio]\nlatency_ms = {value}\n")).unwrap();
        let store = ConfigStore::new(path.clone());

        let config = store.load().unwrap();

        assert_eq!(config.obsolete_fields(), ["audio.latency_ms"], "{value}");
        assert_eq!(config.audio.jitter_buffer_ms, 400, "{value}");
        assert!(config.validate_detection().is_ok(), "{value}");
        assert_eq!(config, AppConfig::default());

        store.save(&config).unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(!raw.contains("latency_ms"), "{raw}");
        assert!(store.load().unwrap().obsolete_fields().is_empty());
    }
}

#[test]
fn both_obsolete_fields_are_reported_together() {
    let config = config_from("[audio]\nvolume = 0.5\nlatency_ms = 500\n");

    assert_eq!(
        config.obsolete_fields(),
        ["audio.volume", "audio.latency_ms"]
    );
    assert!(
        auto_airplay::config::obsolete_field_hint("audio.latency_ms")
            .contains("audio.jitter_buffer_ms")
    );
}

#[test]
fn editing_the_obsolete_latency_does_not_change_the_configuration() {
    let before = audio_from("[audio]\nlatency_ms = 500\n");

    assert_eq!(before, audio_from("[audio]\nlatency_ms = 900\n"));
    assert_eq!(before, audio_from("[audio]\n"));
}

#[test]
fn jitter_buffer_defaults_to_400_ms_and_is_written_by_default() {
    assert_eq!(AppConfig::default().audio.jitter_buffer_ms, 400);
    assert_eq!(audio_from("[audio]\nchannels = 2\n").jitter_buffer_ms, 400);
    let raw = toml::to_string_pretty(&AppConfig::default()).unwrap();
    assert!(raw.contains("jitter_buffer_ms = 400"), "{raw}");
    assert!(!raw.contains("latency_ms"), "{raw}");
}

#[test]
fn jitter_buffer_can_be_configured_up_to_two_seconds_and_more() {
    for value in [200, 400, 2_000, 5_000, 10_000] {
        let config = config_from(&format!("[audio]\njitter_buffer_ms = {value}\n"));

        assert_eq!(config.audio.jitter_buffer_ms, value);
        assert!(config.validate_detection().is_ok(), "{value}");
    }
}

#[test]
fn jitter_buffer_bounds_are_validated() {
    for value in [0, 49, 50, 100, 199, 10_001, 100_000] {
        let config = config_from(&format!("[audio]\njitter_buffer_ms = {value}\n"));

        let message = config.validate_detection().unwrap_err();
        assert!(
            message.contains("jitter_buffer_ms must be between 200 and 10000"),
            "{message}"
        );
    }
}

#[test]
fn jitter_buffer_survives_a_save_and_load() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(directory.path().join("config.toml"));
    let mut config = AppConfig::default();
    config.audio.jitter_buffer_ms = 2_000;

    store.save(&config).unwrap();

    assert_eq!(store.load().unwrap().audio.jitter_buffer_ms, 2_000);
}

#[test]
fn changing_the_jitter_buffer_is_a_configuration_change() {
    let base = audio_from("[audio]\njitter_buffer_ms = 400\n");

    assert_eq!(base, audio_from("[audio]\njitter_buffer_ms = 400\n"));
    assert_ne!(base, audio_from("[audio]\njitter_buffer_ms = 401\n"));
    assert_ne!(base, audio_from("[audio]\njitter_buffer_ms = 2000\n"));
}

// ----- AirPlay 2 buffered mode -----

#[test]
fn stream_mode_defaults_to_realtime_for_existing_configurations() {
    let config = config_from("[output]\nselector = \"Salon\"\nrender_delay_ms = 250\n");

    assert_eq!(config.output.stream_mode, StreamMode::Realtime);
    assert_eq!(config.output.speaker_buffer_ms, DEFAULT_SPEAKER_BUFFER_MS);
    assert_eq!(DEFAULT_SPEAKER_BUFFER_MS, 2_000);
    assert!(config.validate_output().is_ok());
}

#[test]
fn buffered_mode_is_enabled_by_one_key_and_survives_a_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "[output]\nselector = \"Salon\"\nstream_mode = \"buffered\"\n",
    )
    .unwrap();
    let store = ConfigStore::new(path.clone());

    let config = store.load().unwrap();
    assert_eq!(config.output.stream_mode, StreamMode::Buffered);
    assert_eq!(config.output.speaker_buffer_ms, 2_000);

    store.save(&config).unwrap();
    let raw = std::fs::read_to_string(path).unwrap();
    assert!(raw.contains("stream_mode = \"buffered\""), "{raw}");
    assert!(raw.contains("speaker_buffer_ms = 2000"), "{raw}");
    assert_eq!(store.load().unwrap(), config);
}

#[test]
fn default_configuration_writes_the_realtime_mode() {
    let raw = toml::to_string_pretty(&AppConfig::default()).unwrap();
    assert!(raw.contains("stream_mode = \"realtime\""), "{raw}");
}

#[test]
fn unknown_stream_modes_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[output]\nstream_mode = \"burst\"\n").unwrap();

    assert!(ConfigStore::new(path).load().is_err());
}

#[test]
fn buffered_latency_bounds_are_validated() {
    for (value, valid) in [
        (499, false),
        (500, true),
        (2_000, true),
        (10_000, true),
        (10_001, false),
    ] {
        let config = config_from(&format!(
            "[output]\nstream_mode = \"buffered\"\nspeaker_buffer_ms = {value}\n"
        ));
        assert_eq!(config.validate_output().is_ok(), valid, "{value}");
        if !valid {
            let message = config.validate_output().unwrap_err();
            assert!(message.contains("speaker_buffer_ms"), "{message}");
        }
    }
    assert_eq!(SPEAKER_BUFFER_MS_RANGE, 500..=10_000);
}

#[test]
fn switching_the_stream_mode_is_a_configuration_change() {
    let realtime = config_from("[output]\nselector = \"Salon\"\n");
    let buffered = config_from("[output]\nselector = \"Salon\"\nstream_mode = \"buffered\"\n");
    let longer = config_from(
        "[output]\nselector = \"Salon\"\nstream_mode = \"buffered\"\nspeaker_buffer_ms = 3000\n",
    );

    assert_ne!(realtime, buffered);
    assert_ne!(buffered, longer);
}
