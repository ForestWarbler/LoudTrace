use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use loudtrace::audio::{Measurement, analyze_files};

// Reference values and tolerances come from EBU Tech 3341 (November 2023),
// Table 1, rather than from the implementation under test:
// https://tech.ebu.ch/docs/tech/tech3341.pdf
// These checks cover the supported integrated-loudness and true-peak subset;
// they do not assert complete EBU Mode compliance or Spotify equivalence.

fn measure(paths: &[PathBuf]) -> Vec<Measurement> {
    let result = analyze_files(paths, |_, _| {}, &AtomicBool::new(false));
    assert_eq!(result.tracks.len(), paths.len());
    result
        .tracks
        .into_iter()
        .zip(paths)
        .map(|(track, path)| track.unwrap_or_else(|error| panic!("{}: {error}", path.display())))
        .collect()
}

fn assert_loudness(label: &str, measured: &Measurement, expected: f64) {
    let actual = measured.integrated_lufs.expect("Expected finite loudness");
    assert!(
        (actual - expected).abs() <= 0.1,
        "{label}: expected {expected:.1} ±0.1 LUFS, got {actual:.6}"
    );
}

fn assert_true_peak(label: &str, measured: &Measurement, expected: f64) {
    let actual = measured.true_peak_dbtp.expect("Expected finite true peak");
    assert!(
        (expected - 0.4..=expected + 0.2).contains(&actual),
        "{label}: expected {expected:.1} +0.2/−0.4 dBTP, got {actual:.6}"
    );
}

fn write_stereo_signal(
    path: &Path,
    sample_rate: u32,
    frames: usize,
    mut sample: impl FnMut(usize) -> f64,
) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for frame in 0..frames {
        let value = sample(frame) as f32;
        writer.write_sample(value).unwrap();
        writer.write_sample(value).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn published_ebu_relative_and_absolute_gating_signals() {
    let temp = tempfile::tempdir().unwrap();
    let sample_rate = 48_000;
    // Table 1, cases 3–5: quiet regions must be removed by the relative and
    // absolute gates; retained unequal levels must be integrated as energy.
    let cases: &[(&str, &[(f64, f64)])] = &[
        ("case_3", &[(10.0, -36.0), (60.0, -23.0), (10.0, -36.0)]),
        (
            "case_4",
            &[
                (10.0, -72.0),
                (10.0, -36.0),
                (60.0, -23.0),
                (10.0, -36.0),
                (10.0, -72.0),
            ],
        ),
        ("case_5", &[(20.0, -26.0), (20.1, -20.0), (20.0, -26.0)]),
    ];
    let mut paths = Vec::new();
    for (name, segments) in cases {
        let path = temp.path().join(format!("{name}.wav"));
        let mut end = 0;
        let sections: Vec<_> = segments
            .iter()
            .map(|(seconds, peak_dbfs)| {
                end += (seconds * sample_rate as f64).round() as usize;
                (end, 10.0_f64.powf(peak_dbfs / 20.0))
            })
            .collect();
        let mut section = 0;
        write_stereo_signal(&path, sample_rate, end, |frame| {
            while frame >= sections[section].0 {
                section += 1;
            }
            sections[section].1 * (TAU * 1000.0 * frame as f64 / sample_rate as f64).sin()
        });
        paths.push(path);
    }
    for ((name, _), measured) in cases.iter().zip(measure(&paths)) {
        assert_loudness(name, &measured, -23.0);
    }
}

#[test]
fn published_ebu_intersample_true_peak_signals() {
    let temp = tempfile::tempdir().unwrap();
    let sample_rate = 48_000;
    let frames = sample_rate as usize;
    // Table 1, cases 15–19. Fractional-sample phases exercise reconstruction:
    // case 16 has a sample peak near −9 dBFS and a true peak near −6 dBTP.
    let cases = [
        (15, 4.0, 0.50, 0.0_f64, -6.0),
        (16, 4.0, 0.50, 45.0_f64, -6.0),
        (17, 6.0, 0.50, 60.0_f64, -6.0),
        (18, 8.0, 0.50, 67.5_f64, -6.0),
        (19, 4.0, 1.41, 45.0_f64, 3.0),
    ];
    let mut paths = Vec::new();
    for (case, period, amplitude, phase, _) in cases {
        let path = temp.path().join(format!("case_{case}.wav"));
        write_stereo_signal(&path, sample_rate, frames, |frame| {
            let fade_frames = sample_rate as f64 * 0.01;
            let fade = (frame as f64 / fade_frames)
                .min((frames - 1 - frame) as f64 / fade_frames)
                .min(1.0);
            amplitude * fade * (TAU * frame as f64 / period + phase.to_radians()).sin()
        });
        paths.push(path);
    }
    for ((case, _, _, _, expected), measured) in cases.iter().zip(measure(&paths)) {
        assert_true_peak(&format!("case {case}"), &measured, *expected);
    }
}

fn fixture_path(root: &Path, name: &str) -> PathBuf {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        {
            let entry = entry.unwrap();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() && entry.file_name() == name {
                return entry.path();
            }
        }
    }
    panic!("Missing official fixture {name} below {}", root.display());
}

fn fixture_root() -> PathBuf {
    // Obtain and extract the unmodified v5.0 archive outside the repository:
    // https://tech.ebu.ch/publications/ebu_loudness_test_set
    // LOUDTRACE_EBU_FIXTURES=/path/to/extracted/archive cargo test --test ebu_conformance -- --ignored
    PathBuf::from(
        std::env::var_os("LOUDTRACE_EBU_FIXTURES").expect(
            "Set LOUDTRACE_EBU_FIXTURES to the extracted official EBU loudness test set v5.0",
        ),
    )
}

#[test]
#[ignore = "Requires the official EBU v5.0 test archive in LOUDTRACE_EBU_FIXTURES"]
fn official_ebu_integrated_loudness_fixtures() {
    let root = fixture_root();
    // Table 1, cases 1–5 and 7–8. Case 6 is multichannel, outside the app's
    // mono/stereo scope. Cases 7–8 are authentic narrow/wide-range programmes.
    let cases = [
        ("seq-3341-1-16bit.wav", -23.0),
        ("seq-3341-2-16bit.wav", -33.0),
        ("seq-3341-3-16bit-v02.wav", -23.0),
        ("seq-3341-4-16bit-v02.wav", -23.0),
        ("seq-3341-5-16bit-v02.wav", -23.0),
        ("seq-3341-7_seq-3342-5-24bit.wav", -23.0),
        ("seq-3341-2011-8_seq-3342-6-24bit-v02.wav", -23.0),
    ];
    let paths: Vec<_> = cases
        .iter()
        .map(|(name, _)| fixture_path(&root, name))
        .collect();
    for ((name, expected), measured) in cases.iter().zip(measure(&paths)) {
        eprintln!("{name}: {:.6} LUFS", measured.integrated_lufs.unwrap());
        assert_loudness(name, &measured, *expected);
    }
}

#[test]
#[ignore = "Requires the official EBU v5.0 test archive in LOUDTRACE_EBU_FIXTURES"]
fn official_ebu_true_peak_fixtures() {
    let root = fixture_root();
    // Table 1, cases 15–23 include phase-shifted sinusoids and four offsets of
    // a brief intersample peak. The repeated .wav suffix is in the EBU archive.
    let cases = [
        (15, -6.0),
        (16, -6.0),
        (17, -6.0),
        (18, -6.0),
        (19, 3.0),
        (20, 0.0),
        (21, 0.0),
        (22, 0.0),
        (23, 0.0),
    ];
    let paths: Vec<_> = cases
        .iter()
        .map(|(case, _)| fixture_path(&root, &format!("seq-3341-{case}-24bit.wav.wav")))
        .collect();
    for ((case, expected), measured) in cases.iter().zip(measure(&paths)) {
        eprintln!(
            "EBU case {case}: {:.6} dBTP",
            measured.true_peak_dbtp.unwrap()
        );
        assert_true_peak(&format!("case {case}"), &measured, *expected);
    }
}
