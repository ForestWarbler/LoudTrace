#![cfg(target_os = "macos")]

use std::f64::consts::TAU;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use loudtrace::audio::analyze_files;

#[test]
fn independently_encoded_lossless_files_preserve_measurements() {
    // Apple's encoder is independent of Symphonia's decoders. This integration
    // check covers sample scaling, channel order, and declared-duration handling.
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&source, spec).unwrap();
    for frame in 0..88_200 {
        let sample = (10.0_f64.powf(-18.0 / 20.0)
            * (TAU * 1000.0 * frame as f64 / 44_100.0).sin()
            * 8_388_608.0)
            .round() as i32;
        writer.write_sample(sample).unwrap();
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    let mut paths = vec![source.clone()];
    for (extension, container, codec) in [
        ("aiff", "AIFF", "BEI24"),
        ("flac", "flac", "flac"),
        ("m4a", "m4af", "alac"),
    ] {
        let output = directory.path().join(format!("encoded.{extension}"));
        let result = Command::new("/usr/bin/afconvert")
            .arg(&source)
            .arg(&output)
            .args(["-f", container, "-d", codec])
            .output()
            .expect("macOS afconvert must be available for this integration test");
        assert!(
            result.status.success(),
            "{extension}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        paths.push(output);
    }
    let batch = analyze_files(&paths, |_, _| {}, &AtomicBool::new(false));
    let reference = batch.tracks[0].as_ref().unwrap();
    for (path, result) in paths.iter().zip(&batch.tracks) {
        let measured = result
            .as_ref()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(measured.channels, 2);
        assert_eq!(measured.sample_rate, 44_100);
        assert!((measured.duration_seconds - 2.0).abs() < 1e-9);
        assert!(
            (measured.integrated_lufs.unwrap() - reference.integrated_lufs.unwrap()).abs() < 1e-7
        );
        assert!(
            (measured.true_peak_dbtp.unwrap() - reference.true_peak_dbtp.unwrap()).abs() < 1e-7
        );
    }
    assert!(batch.album.is_ok());
}
