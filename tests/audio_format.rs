use auto_airplay::audio::{AudioFormat, FormatCandidate, select_best_format};

#[test]
fn exact_native_rate_wins_over_resampling() {
    let supported = [
        FormatCandidate::new(44_100, 2, 16),
        FormatCandidate::new(48_000, 2, 16),
    ];

    let selected = select_best_format(&supported, Some(48_000)).unwrap();

    assert_eq!(selected, AudioFormat::new(48_000, 2, 16));
}

#[test]
fn highest_quality_stereo_format_is_selected_when_rate_is_unknown() {
    let supported = [
        FormatCandidate::new(32_000, 1, 16),
        FormatCandidate::new(44_100, 2, 16),
        FormatCandidate::new(48_000, 2, 16),
        FormatCandidate::new(96_000, 2, 24),
    ];

    let selected = select_best_format(&supported, None).unwrap();

    assert_eq!(selected, AudioFormat::new(96_000, 2, 24));
}
