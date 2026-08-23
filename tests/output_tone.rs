use auto_airplay::audio::{sine_wave, write_pcm16_wav};

#[test]
fn test_tone_has_the_requested_duration_and_stereo_shape() {
    let samples = sine_wave(48_000, 2, 1_000.0, 0.25);

    assert_eq!(samples.len(), 24_000);
    assert!(samples.iter().any(|sample| *sample != 0));
    for frame in 0..samples.len() / 2 {
        assert_eq!(samples[frame * 2], samples[frame * 2 + 1]);
    }
}

#[test]
fn generated_wav_has_a_valid_pcm_header_and_size() {
    let samples = vec![0_i16, 1, -1, i16::MAX];
    let mut bytes = Vec::new();

    write_pcm16_wav(&mut bytes, 48_000, 2, &samples).unwrap();

    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(&bytes[36..40], b"data");
    assert_eq!(bytes.len(), 44 + samples.len() * 2);
}
