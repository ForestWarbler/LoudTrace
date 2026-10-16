use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ebur128::{EbuR128, Mode};
use symphonia::core::audio::{Channels, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{
    CODEC_TYPE_AAC, CODEC_TYPE_MP1, CODEC_TYPE_MP2, CODEC_TYPE_MP3, CODEC_TYPE_NULL,
    CODEC_TYPE_PCM_ALAW, CODEC_TYPE_PCM_F32BE, CODEC_TYPE_PCM_F32LE, CODEC_TYPE_PCM_F64BE,
    CODEC_TYPE_PCM_F64LE, CODEC_TYPE_PCM_MULAW, CODEC_TYPE_VORBIS, CodecParameters, DecoderOptions,
};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Clone, Debug)]
pub struct Measurement {
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub duration_seconds: f64,
    pub sample_rate: u32,
    pub channels: u32,
}

#[derive(Clone, Debug)]
pub struct AlbumMeasurement {
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

#[derive(Debug)]
pub struct BatchAnalysis {
    pub tracks: Vec<Result<Measurement, String>>,
    pub album: Result<AlbumMeasurement, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: u32,
    pub frames: u64,
}

struct AnalyzedTrack {
    measurement: Measurement,
    loudness: EbuR128,
}

struct Meters {
    loudness: EbuR128,
    peak: EbuR128,
    frames: u64,
}

impl Meters {
    fn new(channels: u32, sample_rate: u32) -> Result<Self, String> {
        if !(1..=2).contains(&channels) {
            return Err("Only mono and stereo audio are supported.".into());
        }
        validate_sample_rate(sample_rate)?;
        // Keep exact gating-block energies instead of quantizing them into a
        // histogram. Mono is measured as one channel, not as duplicated stereo.
        Ok(Self {
            loudness: EbuR128::new(channels, sample_rate, Mode::I)
                .map_err(|err| format!("Cannot initialize loudness measurement: {err}"))?,
            peak: EbuR128::new(channels, sample_rate, Mode::TRUE_PEAK)
                .map_err(|err| format!("Cannot initialize true-peak measurement: {err}"))?,
            frames: 0,
        })
    }

    fn push(&mut self, samples: &[f64]) -> Result<(), String> {
        let channels = self.loudness.channels() as usize;
        validate_pcm_samples(samples, channels)?;
        self.loudness
            .add_frames_f64(samples)
            .map_err(|err| format!("Loudness measurement failed: {err}"))?;
        self.peak
            .add_frames_f64(samples)
            .map_err(|err| format!("True-peak measurement failed: {err}"))?;
        self.frames = self
            .frames
            .checked_add((samples.len() / channels) as u64)
            .ok_or("Audio duration exceeds the supported range.")?;
        Ok(())
    }

    fn finish(mut self) -> Result<AnalyzedTrack, String> {
        if self.frames == 0 {
            return Err("The file contains no decoded audio frames.".into());
        }
        let integrated_lufs = finite_measurement(
            self.loudness
                .loudness_global()
                .map_err(|err| format!("Loudness measurement failed: {err}"))?,
        )?;
        // ebur128 0.1.10 uses a causal 48-tap true-peak interpolator. Flush its
        // tail so a peak near EOF cannot be missed. A separate meter ensures
        // padding never changes duration, integrated loudness, or album blocks.
        self.peak
            .add_frames_f64(&vec![0.0; 64 * self.peak.channels() as usize])
            .map_err(|err| format!("True-peak finalization failed: {err}"))?;
        let mut peak = 0.0_f64;
        for channel in 0..self.peak.channels() {
            let channel_peak = self
                .peak
                .true_peak(channel)
                .map_err(|err| format!("True-peak measurement failed: {err}"))?;
            if !channel_peak.is_finite() || channel_peak < 0.0 {
                return Err("True-peak measurement overflowed.".into());
            }
            peak = peak.max(channel_peak);
        }
        let measurement = Measurement {
            integrated_lufs,
            true_peak_dbtp: (peak > 0.0).then(|| 20.0 * peak.log10()),
            duration_seconds: self.frames as f64 / self.loudness.rate() as f64,
            sample_rate: self.loudness.rate(),
            channels: self.loudness.channels(),
        };
        Ok(AnalyzedTrack {
            measurement,
            loudness: self.loudness,
        })
    }
}

fn finite_measurement(value: f64) -> Result<Option<f64>, String> {
    if value.is_finite() {
        Ok(Some(value))
    } else if value == f64::NEG_INFINITY {
        // Silence, signals below the absolute gate, and files shorter than a
        // full 400 ms gate have no integrated loudness. Do not invent a value.
        Ok(None)
    } else {
        Err("Loudness measurement produced a non-finite result.".into())
    }
}

pub fn analyze_files(
    paths: &[PathBuf],
    mut progress: impl FnMut(usize, usize),
    cancelled: &AtomicBool,
) -> BatchAnalysis {
    let mut tracks = Vec::with_capacity(paths.len());
    let mut successful = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        match analyze_file(path, cancelled) {
            Ok(track) => {
                tracks.push(Ok(track.measurement.clone()));
                successful.push(track);
            }
            Err(error) => tracks.push(Err(error)),
        }
        progress(index + 1, paths.len());
    }
    let album = if cancelled.load(Ordering::Relaxed) {
        Err("Analysis cancelled.".into())
    } else if tracks.is_empty() {
        Err("Add at least one track to analyze an album.".into())
    } else if successful.len() != tracks.len() {
        Err("Every track must be analyzed successfully before calculating the album.".into())
    } else {
        album_measurement(&successful)
    };
    BatchAnalysis { tracks, album }
}

fn album_measurement(tracks: &[AnalyzedTrack]) -> Result<AlbumMeasurement, String> {
    // REFERENCE MODEL, not Spotify's official album implementation. Spotify
    // publicly specifies a common album gain but does not publish its complete
    // album-reference calculation:
    // https://support.spotify.com/us/artists/article/loudness-normalization/
    // Pool each track's 400 ms block energies and apply the absolute/relative
    // gates globally. Never average already-gated track LUFS. Tracks retain
    // independent filter states; no synthetic transitions join their PCM.
    let integrated_lufs = finite_measurement(
        EbuR128::loudness_global_multiple(tracks.iter().map(|track| &track.loudness))
            .map_err(|err| format!("Album loudness calculation failed: {err}"))?,
    )?;
    let true_peak_dbtp = tracks
        .iter()
        .filter_map(|track| track.measurement.true_peak_dbtp)
        .reduce(f64::max);
    Ok(AlbumMeasurement {
        integrated_lufs,
        true_peak_dbtp,
    })
}

fn analyze_file(path: &Path, cancelled: &AtomicBool) -> Result<AnalyzedTrack, String> {
    let mut meters: Option<Meters> = None;
    decode_pcm(path, cancelled, |sample_rate, channels, samples| {
        if meters.is_none() {
            meters = Some(Meters::new(channels, sample_rate)?);
        }
        meters
            .as_mut()
            .ok_or("Missing audio measurement state.")?
            .push(samples)
    })?;
    meters
        .ok_or("The file contains no decoded audio frames.")?
        .finish()
}

// Samples are interleaved and borrowed only for the callback. Decoding retains
// at most one packet's conversion buffer. Consumers must propagate a final
// error: integrity and file-change checks can fail after samples were streamed.
pub fn decode_pcm(
    path: &Path,
    cancelled: &AtomicBool,
    mut on_samples: impl FnMut(u32, u32, &[f64]) -> Result<(), String>,
) -> Result<DecodedAudio, String> {
    check_cancelled(cancelled)?;
    let mut file = File::open(path).map_err(|err| format!("Cannot open audio file: {err}"))?;
    let before = file
        .metadata()
        .map_err(|err| format!("Cannot inspect audio file: {err}"))?;
    if !before.is_file() {
        return Err("The path does not refer to a regular audio file.".into());
    }
    let aiff = inspect_aiff(&mut file, before.len())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| format!("Cannot read audio file: {err}"))?;
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let options = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let mut format = symphonia::default::get_probe()
        .format(&hint, source, &options, &MetadataOptions::default())
        .map_err(|err| format!("Unsupported or damaged audio file: {err}"))?
        .format;
    let track = format
        .default_track()
        .filter(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        })
        .ok_or("No supported audio stream was found.")?;
    let track_id = track.id;
    let mut params = track.codec_params.clone();
    if let Some(aiff) = &aiff {
        aiff.validate_pcm_size(&params)?;
        // Symphonia 0.5.5 includes SSND's eight header bytes in its AIFF
        // duration. COMM is authoritative; independently checked SSND bounds
        // ensure this correction cannot disguise a truncated audio stream.
        params.n_frames = Some(aiff.frames);
    }
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions { verify: true })
        .map_err(|err| format!("Unsupported audio codec: {err}"))?;
    let mut spec: Option<SignalSpec> = None;
    let mut decoded_frames = 0_u64;
    let mut last_packet_end = params.start_ts;
    let mut sample_buffer: Option<SampleBuffer<f64>> = None;
    loop {
        check_cancelled(cancelled)?;
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(DecodeError::IoError(err)) if err.kind() == ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(format!("Cannot read the complete audio stream: {err}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        if has_exact_pcm_timeline(&params) && packet.ts != last_packet_end {
            // Demuxers may resynchronize past damaged frames. Reject gaps or
            // overlaps instead of silently measuring a discontinuous master.
            return Err("The audio stream contains missing or overlapping frames.".into());
        }
        let decoded = decoder
            .decode(&packet)
            // A playback app may skip corrupt packets; an analyzer must fail
            // instead of presenting a measurement for incomplete content.
            .map_err(|err| format!("Audio decoding failed: {err}"))?;
        let current_spec = *decoded.spec();
        if let Some(original) = spec {
            if original != current_spec {
                return Err("The sample rate or channel layout changes within this file.".into());
            }
        } else {
            validate_channels(current_spec.channels)?;
            validate_sample_rate(current_spec.rate)?;
            spec = Some(current_spec);
        }
        let sample_count = decoded.capacity() * current_spec.channels.count();
        if sample_buffer
            .as_ref()
            .is_none_or(|buffer| buffer.capacity() < sample_count)
        {
            sample_buffer = Some(SampleBuffer::<f64>::new(
                decoded.capacity() as u64,
                current_spec,
            ));
        }
        let buffer = sample_buffer
            .as_mut()
            .ok_or("Missing audio conversion buffer.")?;
        buffer.copy_interleaved_ref(decoded);
        let samples = buffer.samples();
        let samples = if let Some(aiff) = &aiff {
            // The same upstream AIFF bug can read up to eight trailing bytes
            // from a following metadata chunk. Only COMM's validated PCM
            // frames belong to the measurement, even if decoding returns more.
            let remaining = aiff.frames.saturating_sub(decoded_frames);
            let frames = remaining.min((samples.len() / current_spec.channels.count()) as u64);
            &samples[..frames as usize * current_spec.channels.count()]
        } else {
            samples
        };
        let channels = current_spec.channels.count();
        validate_pcm_samples(samples, channels)?;
        if !samples.is_empty() {
            check_cancelled(cancelled)?;
            on_samples(current_spec.rate, channels as u32, samples)?;
            decoded_frames = decoded_frames
                .checked_add((samples.len() / channels) as u64)
                .ok_or("Audio duration exceeds the supported range.")?;
        }
        last_packet_end = last_packet_end.max(packet.ts.saturating_add(packet.dur));
        if aiff
            .as_ref()
            .is_some_and(|aiff| decoded_frames == aiff.frames)
        {
            last_packet_end = decoded_frames;
            break;
        }
    }
    check_cancelled(cancelled)?;
    if decoder.finalize().verify_ok == Some(false) {
        return Err("The decoded audio failed its integrity check.".into());
    }
    let spec = spec
        .filter(|_| decoded_frames > 0)
        .ok_or("The file contains no decoded audio frames.")?;
    validate_complete_stream(&params, last_packet_end, decoded_frames, spec.rate)?;
    let after = fs::metadata(path)
        .map_err(|err| format!("The file became unavailable during analysis: {err}"))?;
    if file_changed(&before, &after) {
        return Err("The file changed during analysis. Analyze the completed export again.".into());
    }
    Ok(DecodedAudio {
        sample_rate: spec.rate,
        channels: spec.channels.count() as u32,
        frames: decoded_frames,
    })
}

fn validate_sample_rate(sample_rate: u32) -> Result<(), String> {
    if (8_000..=384_000).contains(&sample_rate) {
        Ok(())
    } else {
        Err(format!("Unsupported sample rate: {sample_rate} Hz."))
    }
}

fn validate_pcm_samples(samples: &[f64], channels: usize) -> Result<(), String> {
    if !samples.len().is_multiple_of(channels) {
        return Err("Decoded audio contains an incomplete sample frame.".into());
    }
    if samples.iter().any(|value| !value.is_finite()) {
        return Err("Audio contains NaN or infinite sample values.".into());
    }
    Ok(())
}

struct AiffBounds {
    frames: u64,
    data_bytes: u64,
}

impl AiffBounds {
    fn validate_pcm_size(&self, params: &CodecParameters) -> Result<(), String> {
        let bytes_per_sample = if let Some(bits) = params.bits_per_coded_sample {
            u64::from(bits.div_ceil(8))
        } else if [CODEC_TYPE_PCM_F32BE, CODEC_TYPE_PCM_F32LE].contains(&params.codec) {
            4
        } else if [CODEC_TYPE_PCM_F64BE, CODEC_TYPE_PCM_F64LE].contains(&params.codec) {
            8
        } else if [CODEC_TYPE_PCM_ALAW, CODEC_TYPE_PCM_MULAW].contains(&params.codec) {
            1
        } else {
            return Err("Unsupported AIFF sample encoding.".into());
        };
        let channels = params
            .channels
            .ok_or("AIFF channel layout is missing.")?
            .count() as u64;
        let expected_bytes = self
            .frames
            .checked_mul(channels)
            .and_then(|size| size.checked_mul(bytes_per_sample));
        if expected_bytes != Some(self.data_bytes) {
            return Err("AIFF sample count does not match its sound-data length.".into());
        }
        Ok(())
    }
}

fn inspect_aiff(file: &mut File, file_length: u64) -> Result<Option<AiffBounds>, String> {
    if file_length < 12 {
        return Ok(None);
    }
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)
        .map_err(|err| format!("Cannot read audio header: {err}"))?;
    if &header[..4] != b"FORM" || (&header[8..12] != b"AIFF" && &header[8..12] != b"AIFC") {
        return Ok(None);
    }
    let form_end = u64::from(u32::from_be_bytes(header[4..8].try_into().unwrap())) + 8;
    if form_end > file_length || form_end < 12 {
        return Err("The AIFF container is truncated or has an invalid length.".into());
    }
    let mut position = 12_u64;
    let mut frames = None;
    while position + 8 <= form_end {
        file.seek(SeekFrom::Start(position))
            .map_err(|err| format!("Cannot read AIFF chunk: {err}"))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)
            .map_err(|err| format!("Cannot read AIFF chunk: {err}"))?;
        let length = u64::from(u32::from_be_bytes(chunk[4..8].try_into().unwrap()));
        let end = position + 8 + length;
        if end > form_end {
            return Err("An AIFF chunk extends beyond the complete file.".into());
        }
        if &chunk[..4] == b"COMM" {
            if length < 18 {
                return Err("The AIFF common header is incomplete.".into());
            }
            let mut common = [0_u8; 6];
            file.read_exact(&mut common)
                .map_err(|err| format!("Cannot read AIFF sample count: {err}"))?;
            frames = Some(u64::from(u32::from_be_bytes(
                common[2..6].try_into().unwrap(),
            )));
        } else if &chunk[..4] == b"SSND" {
            if length < 8 {
                return Err("The AIFF sound-data header is incomplete.".into());
            }
            let mut sound = [0_u8; 8];
            file.read_exact(&mut sound)
                .map_err(|err| format!("Cannot read AIFF sound-data header: {err}"))?;
            if sound != [0; 8] {
                return Err("Block-aligned AIFF audio is not supported.".into());
            }
            return Ok(Some(AiffBounds {
                frames: frames.ok_or("AIFF common header must precede sound data.")?,
                data_bytes: length - 8,
            }));
        }
        position = end + (length & 1);
    }
    Err("The AIFF file has no sound-data chunk.".into())
}

fn validate_channels(channels: Channels) -> Result<(), String> {
    let mono = channels == Channels::FRONT_LEFT || channels == Channels::FRONT_CENTRE;
    let stereo = channels == (Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
    if mono || stereo {
        Ok(())
    } else {
        Err("Only standard mono and stereo channel layouts are supported.".into())
    }
}

fn validate_complete_stream(
    params: &CodecParameters,
    last_packet_end: u64,
    decoded_frames: u64,
    sample_rate: u32,
) -> Result<(), String> {
    // MPEG duration can be a bitrate estimate, and lossy packet durations can
    // include codec priming. Do not mistake those estimates for exact PCM sizes.
    // Lossless containers' declared duration is checked so a premature EOF
    // cannot turn a truncated master into a seemingly valid measurement.
    if !has_exact_pcm_timeline(params) {
        return Ok(());
    }
    if let Some(expected) = params.n_frames.filter(|frames| *frames > 0) {
        if last_packet_end < params.start_ts.saturating_add(expected) {
            return Err(
                "The audio stream ended before its declared duration; the file may be truncated."
                    .into(),
            );
        }
        if let Some(time_base) = params.time_base {
            let time = time_base.calc_time(expected);
            let expected_frames =
                ((time.seconds as f64 + time.frac) * sample_rate as f64).round() as u64;
            if decoded_frames < expected_frames {
                return Err("The decoded audio is shorter than its declared duration; the file may be truncated.".into());
            }
        }
    }
    Ok(())
}

fn has_exact_pcm_timeline(params: &CodecParameters) -> bool {
    ![
        CODEC_TYPE_MP1,
        CODEC_TYPE_MP2,
        CODEC_TYPE_MP3,
        CODEC_TYPE_AAC,
        CODEC_TYPE_VORBIS,
    ]
    .contains(&params.codec)
}

fn file_changed(before: &Metadata, after: &Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return true;
        }
    }
    false
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Analysis cancelled.".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::{FRAC_PI_4, TAU};

    fn tone(rate: u32, channels: u32, seconds: f64, dbfs: f64, frequency: f64) -> Vec<f64> {
        let amplitude = 10.0_f64.powf(dbfs / 20.0);
        (0..(rate as f64 * seconds) as usize)
            .flat_map(|frame| {
                let sample = amplitude * (TAU * frequency * frame as f64 / rate as f64).sin();
                std::iter::repeat_n(sample, channels as usize)
            })
            .collect()
    }

    fn measure(rate: u32, channels: u32, samples: &[f64], chunk_frames: usize) -> AnalyzedTrack {
        let mut meters = Meters::new(channels, rate).unwrap();
        for chunk in samples.chunks(chunk_frames * channels as usize) {
            meters.push(chunk).unwrap();
        }
        meters.finish().unwrap()
    }

    fn near(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected:.6} ± {tolerance:.6}, got {actual:.6}"
        );
    }

    fn write_float_wav(path: &Path, rate: u32, channels: u16, samples: &[f64]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample as f32).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn reference_one_khz_sine_and_mono_channel_weighting() {
        // A stereo 1 kHz sine at -23 dBFS per channel measures approximately
        // -23 LUFS; a single channel is 10*log10(2) LU lower (ITU BS.1770).
        for rate in [44_100, 48_000, 96_000, 192_000] {
            let stereo = measure(rate, 2, &tone(rate, 2, 2.0, -23.0, 1000.0), 997);
            let mono = measure(rate, 1, &tone(rate, 1, 2.0, -23.0, 1000.0), 773);
            near(stereo.measurement.integrated_lufs.unwrap(), -23.0, 0.06);
            near(mono.measurement.integrated_lufs.unwrap(), -26.0103, 0.06);
            near(stereo.measurement.true_peak_dbtp.unwrap(), -23.0, 0.12);
            near(stereo.measurement.duration_seconds, 2.0, f64::EPSILON);
        }
    }

    #[test]
    fn packet_boundaries_do_not_change_measurements() {
        let mut samples = tone(48_000, 2, 1.13, -18.0, 997.0);
        samples.extend(tone(48_000, 2, 0.87, -27.0, 4000.0));
        let whole = measure(48_000, 2, &samples, samples.len() / 2);
        for chunk_frames in [1, 127, 1024, 4800, 8191] {
            let chunked = measure(48_000, 2, &samples, chunk_frames);
            near(
                chunked.measurement.integrated_lufs.unwrap(),
                whole.measurement.integrated_lufs.unwrap(),
                1e-9,
            );
            near(
                chunked.measurement.true_peak_dbtp.unwrap(),
                whole.measurement.true_peak_dbtp.unwrap(),
                1e-9,
            );
        }
    }

    #[test]
    fn relative_and_absolute_gates_exclude_quiet_content() {
        let loud = tone(48_000, 2, 5.0, -23.0, 1000.0);
        let expected = measure(48_000, 2, &loud, 1024)
            .measurement
            .integrated_lufs
            .unwrap();
        let mut mixed = vec![0.0; 48_000 * 2];
        mixed.extend_from_slice(&loud);
        mixed.extend(tone(48_000, 2, 5.0, -43.0, 1000.0));
        mixed.extend(tone(48_000, 2, 1.0, -90.0, 1000.0));
        let result = measure(48_000, 2, &mixed, 1311);
        // With 400 ms windows and 100 ms hops, the loud section contributes
        // 47 full-energy blocks plus three blocks on either boundary. The
        // onset fractions are 1/4, 1/2, 3/4; the ending fractions additionally
        // contain quiet energy at 1/100 of the loud energy. All six transition
        // blocks pass the relative gate, while the purely quiet blocks do not.
        let transition_adjustment = 10.0 * ((47.0_f64 + 1.5 + 1.515) / 53.0).log10();
        near(
            result.measurement.integrated_lufs.unwrap(),
            expected + transition_adjustment,
            0.03,
        );
        let below_gate = measure(48_000, 2, &tone(48_000, 2, 1.0, -90.0, 1000.0), 512);
        assert_eq!(below_gate.measurement.integrated_lufs, None);
        assert!(below_gate.measurement.true_peak_dbtp.unwrap().is_finite());
    }

    #[test]
    fn true_peak_detects_intersample_peaks() {
        let rate = 48_000;
        let samples: Vec<f64> = (0..rate)
            .map(|frame| {
                let fade = (frame as f64 / 1000.0)
                    .min((rate - 1 - frame) as f64 / 1000.0)
                    .min(1.0);
                0.8 * fade * (TAU * 0.25 * frame as f64 + FRAC_PI_4).sin()
            })
            .collect();
        let sample_peak_db = 20.0
            * samples
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0_f64, f64::max)
                .log10();
        let measured = measure(rate, 1, &samples, 127);
        let true_peak = measured.measurement.true_peak_dbtp.unwrap();
        assert!(true_peak > sample_peak_db + 2.5);
        near(true_peak, 20.0 * 0.8_f64.log10(), 0.3);
    }

    #[test]
    fn true_peak_tail_is_measured_without_adding_loudness_blocks() {
        let mut samples = vec![0.0; 100];
        samples.extend([0.7, 0.7]);
        let measured = measure(48_000, 1, &samples, 17);
        let mut explicitly_padded = samples.clone();
        explicitly_padded.extend([0.0; 64]);
        let expected = measure(48_000, 1, &explicitly_padded, 23);
        near(
            measured.measurement.true_peak_dbtp.unwrap(),
            expected.measurement.true_peak_dbtp.unwrap(),
            1e-9,
        );
        assert!(measured.measurement.true_peak_dbtp.unwrap() > 20.0 * 0.7_f64.log10());
        assert_eq!(measured.measurement.integrated_lufs, None);
        near(
            measured.measurement.duration_seconds,
            102.0 / 48_000.0,
            1e-12,
        );
    }

    #[test]
    fn album_uses_block_energy_weighting_and_global_gating() {
        let loud = measure(48_000, 2, &tone(48_000, 2, 2.0, -23.0, 1000.0), 1024);
        let quiet = measure(44_100, 2, &tone(44_100, 2, 8.0, -27.0, 1000.0), 1024);
        let expected = -23.0 + 10.0 * ((17.0 + 77.0 * 10.0_f64.powf(-0.4)) / 94.0).log10();
        let album = album_measurement(&[loud, quiet]).unwrap();
        near(album.integrated_lufs.unwrap(), expected, 0.06);
        near(album.true_peak_dbtp.unwrap(), -23.0, 0.12);
        assert!((album.integrated_lufs.unwrap() - -25.0).abs() > 0.5);

        let loud = measure(48_000, 2, &tone(48_000, 2, 2.0, -23.0, 1000.0), 1024);
        let quiet = measure(96_000, 2, &tone(96_000, 2, 2.0, -43.0, 1000.0), 1024);
        let album = album_measurement(&[loud, quiet]).unwrap();
        near(album.integrated_lufs.unwrap(), -23.0, 0.06);
    }

    #[test]
    fn silence_short_files_and_empty_input_are_explicit() {
        let silent = measure(48_000, 2, &vec![0.0; 96_000], 1024);
        assert_eq!(silent.measurement.integrated_lufs, None);
        assert_eq!(silent.measurement.true_peak_dbtp, None);
        let short = measure(48_000, 1, &tone(48_000, 1, 0.1, -23.0, 1000.0), 1024);
        assert_eq!(short.measurement.integrated_lufs, None);
        assert!(short.measurement.true_peak_dbtp.is_some());
        assert!(Meters::new(1, 48_000).unwrap().finish().is_err());
        assert!(
            analyze_files(&[], |_, _| {}, &AtomicBool::new(false))
                .album
                .is_err()
        );
    }

    #[test]
    fn nonfinite_samples_and_unsupported_layouts_are_rejected() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Meters::new(1, 48_000).unwrap().push(&[value]).is_err());
        }
        assert!(Meters::new(6, 48_000).is_err());
        assert!(validate_channels(Channels::LFE1).is_err());
        assert!(validate_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT).is_ok());
    }

    #[test]
    fn file_decode_matches_pcm_and_reloads_an_overwritten_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("master.wav");
        let samples = tone(48_000, 2, 1.0, -23.0, 1000.0);
        write_float_wav(&path, 48_000, 2, &samples);
        let first = analyze_files(
            std::slice::from_ref(&path),
            |_, _| {},
            &AtomicBool::new(false),
        );
        let first_track = first.tracks[0].as_ref().unwrap();
        near(first_track.integrated_lufs.unwrap(), -23.0, 0.06);
        assert_eq!(first_track.channels, 2);
        assert_eq!(first_track.sample_rate, 48_000);
        near(first_track.duration_seconds, 1.0, 1e-12);

        write_float_wav(&path, 48_000, 2, &tone(48_000, 2, 1.0, -17.0, 1000.0));
        let second = analyze_files(&[path], |_, _| {}, &AtomicBool::new(false));
        near(
            second.tracks[0].as_ref().unwrap().integrated_lufs.unwrap()
                - first_track.integrated_lufs.unwrap(),
            6.0,
            1e-5,
        );
    }

    #[test]
    fn pcm_integer_formats_are_scaled_correctly() {
        let temp = tempfile::tempdir().unwrap();
        for bits in [16, 24, 32] {
            let path = temp.path().join(format!("pcm{bits}.wav"));
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: bits,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            let scale = 2.0_f64.powi(bits as i32 - 1);
            for sample in tone(48_000, 2, 1.0, -23.0, 1000.0) {
                writer
                    .write_sample((sample * scale).round() as i32)
                    .unwrap();
            }
            writer.finalize().unwrap();
            let result = analyze_files(&[path], |_, _| {}, &AtomicBool::new(false));
            near(
                result.tracks[0].as_ref().unwrap().integrated_lufs.unwrap(),
                -23.0,
                0.06,
            );
        }
    }

    #[test]
    fn missing_or_truncated_tracks_block_only_the_album() {
        let temp = tempfile::tempdir().unwrap();
        let valid = temp.path().join("valid.wav");
        let truncated = temp.path().join("truncated.wav");
        write_float_wav(&valid, 48_000, 2, &tone(48_000, 2, 1.0, -23.0, 1000.0));
        fs::copy(&valid, &truncated).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&truncated).unwrap();
        file.set_len(file.metadata().unwrap().len() - 2003).unwrap();
        let paths = [valid, temp.path().join("missing.wav"), truncated];
        let mut progress = Vec::new();
        let batch = analyze_files(
            &paths,
            |done, total| progress.push((done, total)),
            &AtomicBool::new(false),
        );
        assert!(batch.tracks[0].is_ok());
        assert!(batch.tracks[1].is_err());
        assert!(batch.tracks[2].is_err());
        assert!(batch.album.is_err());
        assert_eq!(progress, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn invalid_float_file_and_multichannel_file_fail() {
        let temp = tempfile::tempdir().unwrap();
        let nonfinite = temp.path().join("invalid.wav");
        let multichannel = temp.path().join("surround.wav");
        write_float_wav(&nonfinite, 48_000, 1, &[0.0, f64::NAN, 0.0]);
        write_float_wav(&multichannel, 48_000, 6, &[0.0; 48]);
        let batch = analyze_files(
            &[nonfinite, multichannel],
            |_, _| {},
            &AtomicBool::new(false),
        );
        assert!(batch.tracks.iter().all(Result::is_err));
        assert!(batch.album.is_err());
    }

    #[test]
    fn cancellation_never_publishes_an_album() {
        let batch = analyze_files(
            &[
                PathBuf::from("not-opened.wav"),
                PathBuf::from("not-opened-either.wav"),
            ],
            |_, _| {},
            &AtomicBool::new(true),
        );
        assert_eq!(batch.tracks.len(), 2);
        assert!(
            batch
                .tracks
                .iter()
                .all(|track| track.as_ref().unwrap_err() == "Analysis cancelled.")
        );
        assert!(batch.album.is_err());
    }

    fn aiff_pcm16_with_metadata(samples: &[i16], channels: u16) -> Vec<u8> {
        let mut body = b"AIFFCOMM".to_vec();
        body.extend(18_u32.to_be_bytes());
        body.extend(channels.to_be_bytes());
        body.extend(((samples.len() / channels as usize) as u32).to_be_bytes());
        body.extend(16_u16.to_be_bytes());
        // IEEE 80-bit extended representation of 44100 Hz.
        body.extend([0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]);
        body.extend(b"SSND");
        body.extend((8 + samples.len() as u32 * 2).to_be_bytes());
        body.extend([0_u8; 8]);
        for sample in samples {
            body.extend(sample.to_be_bytes());
        }
        body.extend(b"ANNO");
        body.extend(8_u32.to_be_bytes());
        body.extend(b"metadata");
        let mut file = b"FORM".to_vec();
        file.extend((body.len() as u32).to_be_bytes());
        file.extend(body);
        file
    }

    #[test]
    fn aiff_uses_all_declared_samples_and_excludes_following_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.aiff");
        let pcm: Vec<i16> = tone(44_100, 2, 1.0, -23.0, 1000.0)
            .into_iter()
            .map(|sample| (sample * 32768.0).round() as i16)
            .collect();
        fs::write(&path, aiff_pcm16_with_metadata(&pcm, 2)).unwrap();
        let expected_samples: Vec<f64> =
            pcm.iter().map(|sample| *sample as f64 / 32768.0).collect();
        let expected = measure(44_100, 2, &expected_samples, 1152).measurement;
        let result = analyze_files(&[path], |_, _| {}, &AtomicBool::new(false));
        let actual = result.tracks[0].as_ref().unwrap();
        near(actual.duration_seconds, 1.0, 1e-12);
        near(
            actual.integrated_lufs.unwrap(),
            expected.integrated_lufs.unwrap(),
            1e-9,
        );
        near(
            actual.true_peak_dbtp.unwrap(),
            expected.true_peak_dbtp.unwrap(),
            1e-9,
        );
        assert!(result.album.is_ok());
    }

    #[test]
    fn aiff_rejects_truncation_and_inconsistent_frame_counts() {
        let directory = tempfile::tempdir().unwrap();
        let complete = aiff_pcm16_with_metadata(&[100; 400], 2);
        let truncated_path = directory.path().join("truncated.aiff");
        // Remove one PCM byte and all following metadata. Preserve declared
        // container and sound-data lengths so physical truncation is detected.
        fs::write(&truncated_path, &complete[..complete.len() - 17]).unwrap();
        let bad_count_path = directory.path().join("bad-count.aiff");
        let mut bad_count = complete.clone();
        bad_count[22..26].copy_from_slice(&201_u32.to_be_bytes());
        fs::write(&bad_count_path, bad_count).unwrap();
        let bad_sound_path = directory.path().join("bad-sound.aiff");
        let mut bad_sound = complete;
        bad_sound[42..46].copy_from_slice(&804_u32.to_be_bytes());
        fs::write(&bad_sound_path, bad_sound).unwrap();
        let batch = analyze_files(
            &[truncated_path, bad_count_path, bad_sound_path],
            |_, _| {},
            &AtomicBool::new(false),
        );
        assert!(batch.tracks.iter().all(Result::is_err));
        assert!(batch.album.is_err());
    }

    #[test]
    fn streaming_decode_preserves_pcm_frames_and_measured_duration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.wav");
        let original = tone(48_000, 2, 0.43, -18.0, 997.0);
        write_float_wav(&path, 48_000, 2, &original);
        let mut received = Vec::new();
        let mut chunks = 0;
        let decoded = decode_pcm(&path, &AtomicBool::new(false), |rate, channels, samples| {
            assert_eq!((rate, channels), (48_000, 2));
            assert!(!samples.is_empty());
            assert!(samples.len() < original.len());
            chunks += 1;
            received.extend_from_slice(samples);
            Ok(())
        })
        .unwrap();
        assert!(chunks > 1);
        assert_eq!(
            decoded,
            DecodedAudio {
                sample_rate: 48_000,
                channels: 2,
                frames: 20_640
            }
        );
        let expected: Vec<f64> = original
            .into_iter()
            .map(|sample| sample as f32 as f64)
            .collect();
        assert_eq!(received, expected);
        let batch = analyze_files(&[path], |_, _| {}, &AtomicBool::new(false));
        near(
            batch.tracks[0].as_ref().unwrap().duration_seconds,
            decoded.frames as f64 / decoded.sample_rate as f64,
            1e-12,
        );
    }

    #[test]
    fn streaming_aiff_decode_excludes_trailing_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.aiff");
        let original: Vec<i16> = (0..1234).map(|frame| (frame % 257) - 128).collect();
        fs::write(&path, aiff_pcm16_with_metadata(&original, 1)).unwrap();
        let mut received = Vec::new();
        let decoded = decode_pcm(&path, &AtomicBool::new(false), |rate, channels, samples| {
            assert_eq!((rate, channels), (44_100, 1));
            received.extend_from_slice(samples);
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded.frames, original.len() as u64);
        let expected: Vec<f64> = original
            .iter()
            .map(|sample| *sample as f64 / 32768.0)
            .collect();
        assert_eq!(received, expected);
    }

    #[test]
    fn streaming_decode_honors_cancellation_and_consumer_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cancel.wav");
        write_float_wav(&path, 48_000, 1, &tone(48_000, 1, 0.1, -23.0, 1000.0));
        let cancelled = AtomicBool::new(true);
        let mut callbacks = 0;
        let result = decode_pcm(&path, &cancelled, |_, _, _| {
            callbacks += 1;
            Ok(())
        });
        assert_eq!(result.unwrap_err(), "Analysis cancelled.");
        assert_eq!(callbacks, 0);

        cancelled.store(false, Ordering::Relaxed);
        let result = decode_pcm(&path, &cancelled, |_, _, _| {
            callbacks += 1;
            cancelled.store(true, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(result.unwrap_err(), "Analysis cancelled.");
        assert_eq!(callbacks, 1);

        cancelled.store(false, Ordering::Relaxed);
        let result = decode_pcm(&path, &cancelled, |_, _, _| Err("consumer stopped".into()));
        assert_eq!(result.unwrap_err(), "consumer stopped");
    }

    #[test]
    fn streaming_decode_rejects_nonfinite_samples_before_callback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid-stream.wav");
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            write_float_wav(&path, 48_000, 1, &[0.25, invalid, -0.25]);
            let mut received_samples = 0;
            let result = decode_pcm(&path, &AtomicBool::new(false), |_, _, samples| {
                received_samples += samples.len();
                Ok(())
            });
            assert_eq!(
                result.unwrap_err(),
                "Audio contains NaN or infinite sample values."
            );
            assert_eq!(received_samples, 0);
        }
    }
}
