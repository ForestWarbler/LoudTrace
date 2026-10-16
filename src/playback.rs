use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use rubato::{FftFixedInOut, Resampler};

use crate::audio::decode_pcm;
use crate::project::{FileStamp, file_stamp};

const NO_SEEK: u64 = u64::MAX;
const IO_BUFFER: usize = 64 * 1024;
type ErrorSlot = Arc<Mutex<Option<String>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Loading,
    Playing,
    Paused,
    Finished,
}

struct Controls {
    normalization: AtomicU32,
    monitor: AtomicU32,
    seek_frame: AtomicU64,
    position_frame: AtomicU64,
}

impl Controls {
    fn new(gain: f32, volume: f32) -> Self {
        Self {
            normalization: AtomicU32::new(gain.to_bits()),
            monitor: AtomicU32::new(volume.to_bits()),
            seek_frame: AtomicU64::new(NO_SEEK),
            position_frame: AtomicU64::new(0),
        }
    }
}

struct LoadJob {
    cancelled: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Result<Ready, String>>,
}

struct Ready {
    output: MixerDeviceSink,
    pcm: PcmCache,
    errors: ErrorSlot,
}

pub struct Playback {
    state: PlaybackState,
    job: Option<LoadJob>,
    player: Option<Player>,
    output: Option<MixerDeviceSink>,
    controls: Arc<Controls>,
    errors: ErrorSlot,
    error: Option<String>,
    duration: Duration,
    sample_rate: u32,
    gain: f32,
    volume: f32,
}

impl Default for Playback {
    fn default() -> Self {
        Self::new()
    }
}

impl Playback {
    pub fn new() -> Self {
        Self {
            state: PlaybackState::Stopped,
            job: None,
            player: None,
            output: None,
            controls: Arc::new(Controls::new(1.0, 0.25)),
            errors: Arc::new(Mutex::new(None)),
            error: None,
            duration: Duration::ZERO,
            sample_rate: 1,
            gain: 1.0,
            volume: 0.25,
        }
    }

    pub fn load(&mut self, path: PathBuf, expected: FileStamp, gain_db: f64) -> Result<(), String> {
        let gain = linear_gain(gain_db)?;
        require_stamp(&path, &expected)?;
        // Keep the output stream alive across natural playlist transitions.
        // The previous track's final hardware buffer must finish playing even
        // though the source iterator is already empty.
        let reusable_output = if self.state == PlaybackState::Finished {
            self.output
                .take()
                .map(|output| (output, Arc::clone(&self.errors)))
        } else {
            None
        };
        self.stop();
        self.error = None;
        self.gain = gain;
        self.controls = Arc::new(Controls::new(gain, self.volume));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = prepare_playback(&path, &expected, &worker_cancelled, reusable_output);
            if !worker_cancelled.load(Ordering::Relaxed) {
                let _ = sender.send(result);
            }
        });
        self.job = Some(LoadJob {
            cancelled,
            receiver,
        });
        self.state = PlaybackState::Loading;
        Ok(())
    }

    pub fn poll(&mut self) {
        if let Some(job) = &self.job {
            match job.receiver.try_recv() {
                Ok(Ok(ready)) => {
                    self.job = None;
                    self.duration = ready.pcm.duration();
                    self.sample_rate = ready.pcm.sample_rate;
                    self.errors = ready.errors;
                    let source = PcmSource::new(
                        ready.pcm,
                        Arc::clone(&self.controls),
                        Arc::clone(&self.errors),
                    );
                    let player = Player::connect_new(ready.output.mixer());
                    player.append(source);
                    self.player = Some(player);
                    self.output = Some(ready.output);
                    self.state = PlaybackState::Playing;
                }
                Ok(Err(error)) => self.fail(error),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fail("The playback preparation worker stopped unexpectedly.".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let stream_error = self.errors.lock().ok().and_then(|mut error| error.take());
        if let Some(error) = stream_error {
            self.fail(error);
            return;
        }
        if self.state == PlaybackState::Playing && self.player.as_ref().is_some_and(Player::empty) {
            self.state = PlaybackState::Finished;
        }
    }

    fn fail(&mut self, error: String) {
        self.stop();
        self.error = Some(error);
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn position(&self) -> Duration {
        if self.state == PlaybackState::Finished {
            return self.duration;
        }
        let pending = self.controls.seek_frame.load(Ordering::Relaxed);
        let frame = if pending == NO_SEEK {
            self.controls.position_frame.load(Ordering::Relaxed)
        } else {
            pending
        };
        Duration::from_secs_f64(frame as f64 / self.sample_rate as f64).min(self.duration)
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            if let Some(player) = &self.player {
                player.pause();
            }
            self.state = PlaybackState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.state == PlaybackState::Paused {
            if let Some(player) = &self.player {
                player.play();
            }
            self.state = PlaybackState::Playing;
        }
    }

    pub fn stop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancelled.store(true, Ordering::Relaxed);
        }
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.output = None;
        self.state = PlaybackState::Stopped;
        self.duration = Duration::ZERO;
        self.controls.seek_frame.store(NO_SEEK, Ordering::Relaxed);
        self.controls.position_frame.store(0, Ordering::Relaxed);
        self.errors = Arc::new(Mutex::new(None));
    }

    pub fn seek(&mut self, position: Duration) -> Result<(), String> {
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused) {
            return Err("Wait for the track to load before seeking.".into());
        }
        let frame =
            (position.min(self.duration).as_secs_f64() * self.sample_rate as f64).round() as u64;
        // Do not use Player::try_seek: it waits for an audio callback and can
        // block indefinitely after a device disconnect. The source consumes
        // this request at the next frame, including after resuming a pause.
        self.controls.seek_frame.store(frame, Ordering::Relaxed);
        Ok(())
    }

    pub fn set_gain_db(&mut self, gain_db: f64) -> Result<(), String> {
        self.gain = linear_gain(gain_db)?;
        self.controls
            .normalization
            .store(self.gain.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.controls
            .monitor
            .store(self.volume.to_bits(), Ordering::Relaxed);
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

fn linear_gain(db: f64) -> Result<f32, String> {
    let gain = 10_f64.powf(db / 20.0) as f32;
    if !db.is_finite() || !gain.is_finite() || gain <= 0.0 {
        return Err("The playback gain must be finite and representable.".into());
    }
    Ok(gain)
}

fn record_error(errors: &ErrorSlot, message: String) {
    if let Ok(mut error) = errors.lock()
        && error.is_none()
    {
        *error = Some(message);
    }
}

fn require_stamp(path: &Path, expected: &FileStamp) -> Result<(), String> {
    if file_stamp(path).as_ref() != Ok(expected) {
        return Err(
            "The source file changed or is unavailable. Analyze it again before listening.".into(),
        );
    }
    Ok(())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Playback preparation cancelled.".into())
    } else {
        Ok(())
    }
}

fn prepare_playback(
    path: &Path,
    expected: &FileStamp,
    cancelled: &AtomicBool,
    reusable_output: Option<(MixerDeviceSink, ErrorSlot)>,
) -> Result<Ready, String> {
    check_cancelled(cancelled)?;
    require_stamp(path, expected)?;
    let (mut output, errors) = if let Some(output) = reusable_output {
        output
    } else {
        let errors: ErrorSlot = Arc::new(Mutex::new(None));
        let callback_errors = Arc::clone(&errors);
        let output = DeviceSinkBuilder::from_default_device()
            .map_err(|error| format!("Cannot find an audio output device: {error}"))?
            .with_channels(rodio::nz!(2))
            .with_error_callback(move |error| {
                record_error(&callback_errors, format!("Audio output stopped: {error}"));
            })
            .open_stream()
            .map_err(|error| format!("Cannot open stereo audio output: {error}"))?;
        (output, errors)
    };
    output.log_on_drop(false);
    check_cancelled(cancelled)?;
    let pcm = prepare_pcm(
        path,
        expected,
        output.config().sample_rate().get(),
        cancelled,
    )?;
    Ok(Ready {
        output,
        pcm,
        errors,
    })
}

struct PcmCache {
    file: File,
    sample_rate: u32,
    channels: u32,
    frames: u64,
}

impl PcmCache {
    fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames as f64 / self.sample_rate as f64)
    }
}

fn prepare_pcm(
    path: &Path,
    expected: &FileStamp,
    output_rate: u32,
    cancelled: &AtomicBool,
) -> Result<PcmCache, String> {
    check_cancelled(cancelled)?;
    require_stamp(path, expected)?;
    let file =
        tempfile::tempfile().map_err(|error| format!("Cannot create audio cache: {error}"))?;
    let mut writer = BufWriter::with_capacity(IO_BUFFER, file);
    let decoded = decode_pcm(path, cancelled, |_, _, samples| {
        check_cancelled(cancelled)?;
        for &sample in samples {
            let sample = sample as f32;
            if !sample.is_finite() {
                return Err("A decoded audio sample exceeds the playback range.".into());
            }
            writer
                .write_all(&sample.to_le_bytes())
                .map_err(|error| format!("Cannot write audio cache: {error}"))?;
        }
        Ok(())
    })?;
    writer
        .flush()
        .map_err(|error| format!("Cannot finish audio cache: {error}"))?;
    let mut file = writer
        .into_inner()
        .map_err(|error| format!("Cannot finish audio cache: {error}"))?;
    file.rewind()
        .map_err(|error| format!("Cannot rewind audio cache: {error}"))?;
    let pcm = PcmCache {
        file,
        sample_rate: decoded.sample_rate,
        channels: decoded.channels,
        frames: decoded.frames,
    };
    let pcm = if pcm.sample_rate == output_rate {
        pcm
    } else {
        resample_pcm(pcm, output_rate, cancelled)?
    };
    check_cancelled(cancelled)?;
    require_stamp(path, expected)?;
    Ok(pcm)
}

fn resample_pcm(
    pcm: PcmCache,
    output_rate: u32,
    cancelled: &AtomicBool,
) -> Result<PcmCache, String> {
    if output_rate == 0 {
        return Err("The audio output device reported an invalid sample rate.".into());
    }
    // Rodio's implicit rate conversion uses linear interpolation/sample
    // dropping. Prepare band-limited PCM at the actual device rate instead.
    // Remove the resampler delay and trim the zero-padded tail to preserve
    // the source timeline and exact seeking across mixed-rate albums.
    let channels = pcm.channels as usize;
    let mut divisor = pcm.sample_rate;
    let mut remainder = output_rate;
    while remainder != 0 {
        (divisor, remainder) = (remainder, divisor % remainder);
    }
    let ratio_unit = (pcm.sample_rate / divisor) as usize;
    // An even number of ratio units gives an integral group delay on both
    // sides. Odd output FFT sizes otherwise leave a half-sample phase offset
    // because rubato reports its output delay as an integer.
    let chunk = 2048_usize.div_ceil(ratio_unit).next_multiple_of(2) * ratio_unit;
    let mut resampler = FftFixedInOut::<f64>::new(
        pcm.sample_rate as usize,
        output_rate as usize,
        chunk,
        channels,
    )
    .map_err(|error| format!("Cannot initialize playback resampling: {error}"))?;
    let mut input = resampler.input_buffer_allocate(true);
    let mut output = resampler.output_buffer_allocate(true);
    let mut reader = BufReader::with_capacity(IO_BUFFER, pcm.file);
    let file =
        tempfile::tempfile().map_err(|error| format!("Cannot create audio cache: {error}"))?;
    let mut writer = BufWriter::with_capacity(IO_BUFFER, file);
    let mut remaining_input = pcm.frames;
    let output_frames = ((pcm.frames as u128 * output_rate as u128 + pcm.sample_rate as u128 / 2)
        / pcm.sample_rate as u128) as u64;
    let mut remaining_output = output_frames;
    let mut skip = resampler.output_delay();
    while remaining_output > 0 {
        check_cancelled(cancelled)?;
        let available = remaining_input.min(resampler.input_frames_next() as u64) as usize;
        for channel in &mut input {
            channel.fill(0.0);
        }
        for frame in 0..available {
            for channel in &mut input {
                let mut bytes = [0; 4];
                reader
                    .read_exact(&mut bytes)
                    .map_err(|error| format!("Cannot read audio cache: {error}"))?;
                channel[frame] = f32::from_le_bytes(bytes) as f64;
            }
        }
        remaining_input -= available as u64;
        let (_, produced) = resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|error| format!("Playback resampling failed: {error}"))?;
        let start = skip.min(produced);
        skip -= start;
        let count = ((produced - start) as u64).min(remaining_output) as usize;
        for frame in start..start + count {
            for channel in &output {
                let sample = channel[frame] as f32;
                if !sample.is_finite() {
                    return Err("Resampling produced a non-finite audio sample.".into());
                }
                writer
                    .write_all(&sample.to_le_bytes())
                    .map_err(|error| format!("Cannot write resampled audio cache: {error}"))?;
            }
        }
        remaining_output -= count as u64;
    }
    writer
        .flush()
        .map_err(|error| format!("Cannot finish resampled audio cache: {error}"))?;
    let mut file = writer
        .into_inner()
        .map_err(|error| format!("Cannot finish resampled audio cache: {error}"))?;
    file.rewind()
        .map_err(|error| format!("Cannot rewind resampled audio cache: {error}"))?;
    Ok(PcmCache {
        file,
        sample_rate: output_rate,
        channels: pcm.channels,
        frames: output_frames,
    })
}

struct Ramp {
    value: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Ramp {
    fn new(value: f32) -> Self {
        Self {
            value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    fn next(&mut self, target: f32, frames: u32) -> f32 {
        if target != self.target {
            self.target = target;
            self.remaining = frames;
            self.step = (target - self.value) / frames as f32;
        }
        if self.remaining > 0 {
            self.remaining -= 1;
            self.value = if self.remaining == 0 {
                self.target
            } else {
                self.value + self.step
            };
        }
        self.value
    }
}

struct PcmSource {
    reader: BufReader<File>,
    sample_rate: u32,
    channels: u32,
    frames: u64,
    sample_index: u64,
    controls: Arc<Controls>,
    errors: ErrorSlot,
    normalization: Ramp,
    monitor: Ramp,
    frame_gain: f32,
    failed: bool,
}

impl PcmSource {
    fn new(pcm: PcmCache, controls: Arc<Controls>, errors: ErrorSlot) -> Self {
        let gain = f32::from_bits(controls.normalization.load(Ordering::Relaxed));
        let volume = f32::from_bits(controls.monitor.load(Ordering::Relaxed));
        Self {
            reader: BufReader::with_capacity(IO_BUFFER, pcm.file),
            sample_rate: pcm.sample_rate,
            channels: pcm.channels,
            frames: pcm.frames,
            sample_index: 0,
            controls,
            errors,
            normalization: Ramp::new(gain),
            monitor: Ramp::new(volume),
            frame_gain: gain * volume,
            failed: false,
        }
    }

    fn seek_frame(&mut self, frame: u64) -> Result<(), std::io::Error> {
        let frame = frame.min(self.frames);
        self.reader
            .seek(SeekFrom::Start(frame * self.channels as u64 * 4))?;
        self.sample_index = frame * self.channels as u64;
        self.controls.position_frame.store(frame, Ordering::Relaxed);
        Ok(())
    }
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if self.sample_index.is_multiple_of(self.channels as u64) {
            let requested = self.controls.seek_frame.swap(NO_SEEK, Ordering::Relaxed);
            if requested != NO_SEEK
                && let Err(error) = self.seek_frame(requested)
            {
                record_error(&self.errors, format!("Cannot seek audio cache: {error}"));
                self.failed = true;
                return None;
            }
            // Both gains ramp over 5 ms, once per frame, so mode and volume
            // changes avoid zipper noise without shifting stereo balance.
            let ramp_frames = (self.sample_rate / 200).max(1);
            let gain = f32::from_bits(self.controls.normalization.load(Ordering::Relaxed));
            let volume = f32::from_bits(self.controls.monitor.load(Ordering::Relaxed));
            self.frame_gain =
                self.normalization.next(gain, ramp_frames) * self.monitor.next(volume, ramp_frames);
        }
        if self.sample_index >= self.frames * self.channels as u64 {
            return None;
        }
        let mut bytes = [0; 4];
        if let Err(error) = self.reader.read_exact(&mut bytes) {
            record_error(
                &self.errors,
                format!("Cannot read playback audio cache: {error}"),
            );
            self.failed = true;
            return None;
        }
        let sample = f32::from_le_bytes(bytes) * self.frame_gain;
        if !sample.is_finite() {
            record_error(
                &self.errors,
                "The playback signal exceeds the supported range.".into(),
            );
            self.failed = true;
            return None;
        }
        self.sample_index += 1;
        if self.sample_index.is_multiple_of(self.channels as u64) {
            self.controls
                .position_frame
                .store(self.sample_index / self.channels as u64, Ordering::Relaxed);
        }
        Some(sample)
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        std::num::NonZeroU16::new(self.channels as u16).unwrap()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        std::num::NonZeroU32::new(self.sample_rate).unwrap()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(Duration::from_secs_f64(
            self.frames as f64 / self.sample_rate as f64,
        ))
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
        let frame = (position.as_secs_f64() * self.sample_rate as f64).round() as u64;
        self.seek_frame(frame)
            .map_err(|error| rodio::source::SeekError::Other(Arc::new(error)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(samples: &[f32], sample_rate: u32, channels: u32) -> PcmCache {
        assert!(samples.len().is_multiple_of(channels as usize));
        let mut file = tempfile::tempfile().unwrap();
        for sample in samples {
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
        file.rewind().unwrap();
        PcmCache {
            file,
            sample_rate,
            channels,
            frames: (samples.len() / channels as usize) as u64,
        }
    }

    fn source(pcm: PcmCache, gain: f32, volume: f32) -> PcmSource {
        PcmSource::new(
            pcm,
            Arc::new(Controls::new(gain, volume)),
            Arc::new(Mutex::new(None)),
        )
    }

    fn read_cache(pcm: PcmCache) -> Vec<f32> {
        source(pcm, 1.0, 1.0).collect()
    }

    #[test]
    fn normalization_and_monitor_gain_are_independent_without_intermediate_clipping() {
        let gain = linear_gain(20.0 * 2_f64.log10()).unwrap();
        let samples = [0.75, -0.75, 0.25, -0.25];
        let normalized = source(cache(&samples, 48_000, 2), gain, 0.25).collect::<Vec<_>>();
        assert_eq!(normalized, [0.375, -0.375, 0.125, -0.125]);
        let original = source(cache(&samples, 48_000, 2), 1.0, 0.25).collect::<Vec<_>>();
        assert_eq!(original, [0.1875, -0.1875, 0.0625, -0.0625]);
        assert!(source(cache(&samples, 48_000, 2), gain, 0.0).all(|sample| sample == 0.0));
    }

    #[test]
    fn gain_changes_ramp_per_frame_and_preserve_stereo_balance() {
        let mut source = source(cache(&vec![1.0; 600], 48_000, 2), 1.0, 1.0);
        assert_eq!(source.next(), Some(1.0));
        source
            .controls
            .normalization
            .store(0.5_f32.to_bits(), Ordering::Relaxed);
        source
            .controls
            .monitor
            .store(0.25_f32.to_bits(), Ordering::Relaxed);
        assert_eq!(source.next(), Some(1.0));
        for _ in 0..240 {
            let left = source.next().unwrap();
            let right = source.next().unwrap();
            assert_eq!(left, right);
            assert!((0.125..1.0).contains(&left) || left == 0.125);
        }
        assert_eq!(source.next(), Some(0.125));
    }

    #[test]
    fn seek_is_frame_aligned_in_both_directions_and_saturates_at_eof() {
        let mut source = source(cache(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 10, 2), 1.0, 1.0);
        assert_eq!(source.next(), Some(0.1));
        source.controls.seek_frame.store(2, Ordering::Relaxed);
        assert_eq!(source.next(), Some(0.2));
        assert_eq!(source.next(), Some(0.5));
        assert_eq!(source.next(), Some(0.6));
        source.try_seek(Duration::ZERO).unwrap();
        assert_eq!(source.next(), Some(0.1));
        source.try_seek(Duration::from_secs(99)).unwrap();
        assert_eq!(source.next(), None);
        assert_eq!(source.controls.position_frame.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn truncated_playback_cache_reports_an_error_instead_of_clean_eof() {
        let pcm = cache(&[0.1, 0.2], 48_000, 1);
        pcm.file.set_len(4).unwrap();
        let mut source = source(pcm, 1.0, 1.0);
        assert_eq!(source.next(), Some(0.1));
        assert_eq!(source.next(), None);
        assert!(
            source
                .errors
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .contains("Cannot read playback")
        );
    }

    #[test]
    fn resampling_preserves_duration_tone_level_and_frequency() {
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100)] {
            let samples = (0..input_rate)
                .map(|frame| {
                    (std::f64::consts::TAU * 1000.0 * frame as f64 / input_rate as f64).sin() as f32
                        * 0.25
                })
                .collect::<Vec<_>>();
            let resampled = resample_pcm(
                cache(&samples, input_rate, 1),
                output_rate,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(resampled.frames, output_rate as u64);
            assert_eq!(resampled.duration(), Duration::from_secs(1));
            let samples = read_cache(resampled);
            let middle = &samples[output_rate as usize / 10..output_rate as usize * 9 / 10];
            let mean_square = middle
                .iter()
                .map(|sample| (*sample as f64).powi(2))
                .sum::<f64>()
                / middle.len() as f64;
            assert!((mean_square.sqrt() - 0.25 / 2_f64.sqrt()).abs() < 0.0001);
            let error = middle
                .iter()
                .enumerate()
                .map(|(index, sample)| {
                    let frame = index + output_rate as usize / 10;
                    let expected =
                        (std::f64::consts::TAU * 1000.0 * frame as f64 / output_rate as f64).sin()
                            * 0.25;
                    (*sample as f64 - expected).powi(2)
                })
                .sum::<f64>()
                / middle.len() as f64;
            assert!(
                error.sqrt() < 0.0001,
                "rate {input_rate} -> {output_rate}, RMS error {}",
                error.sqrt()
            );
        }
    }

    #[test]
    fn downsampling_suppresses_out_of_band_aliases() {
        let samples = (0..96_000)
            .map(|frame| {
                (std::f64::consts::TAU * 30_000.0 * frame as f64 / 96_000.0).sin() as f32 * 0.5
            })
            .collect::<Vec<_>>();
        let resampled =
            resample_pcm(cache(&samples, 96_000, 1), 48_000, &AtomicBool::new(false)).unwrap();
        let samples = read_cache(resampled);
        let middle = &samples[4800..43200];
        let rms = (middle
            .iter()
            .map(|sample| (*sample as f64).powi(2))
            .sum::<f64>()
            / middle.len() as f64)
            .sqrt();
        assert!(rms < 0.00001, "out-of-band RMS {rms}");
    }

    #[test]
    fn resampler_flushes_partial_blocks_and_preserves_channel_isolation() {
        for frames in [1, 2047, 2048, 2049, 2204, 2205, 2206] {
            let mut samples = vec![0.0; frames * 2];
            samples[(frames - 1) * 2] = 0.5;
            let resampled =
                resample_pcm(cache(&samples, 44_100, 2), 48_000, &AtomicBool::new(false)).unwrap();
            let expected_frames = (frames * 48_000 + 44_100 / 2) / 44_100;
            assert_eq!(resampled.frames, expected_frames as u64);
            let samples = read_cache(resampled);
            assert_eq!(samples.len(), expected_frames * 2);
            assert!(
                samples
                    .iter()
                    .skip(1)
                    .step_by(2)
                    .all(|sample| *sample == 0.0)
            );
            assert!(
                samples
                    .iter()
                    .rev()
                    .take(12)
                    .any(|sample| sample.abs() > 0.1),
                "lost final impulse for {frames} frames"
            );
        }
    }

    #[test]
    fn preparation_uses_verified_decoder_and_rejects_stale_or_cancelled_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for sample in [0.1_f32, -0.2, 0.3, -0.4] {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        let stamp = file_stamp(&path).unwrap();
        let pcm = prepare_pcm(&path, &stamp, 48_000, &AtomicBool::new(false)).unwrap();
        assert_eq!(read_cache(pcm), [0.1, -0.2, 0.3, -0.4]);
        assert!(prepare_pcm(&path, &stamp, 48_000, &AtomicBool::new(true)).is_err());
        std::fs::write(&path, b"changed").unwrap();
        assert!(
            prepare_pcm(&path, &stamp, 48_000, &AtomicBool::new(false))
                .err()
                .unwrap()
                .contains("Analyze it again")
        );
    }

    #[test]
    fn stop_cancels_preparation_and_discards_late_worker_results() {
        let mut playback = Playback::new();
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        playback.job = Some(LoadJob {
            cancelled: Arc::clone(&cancelled),
            receiver,
        });
        playback.state = PlaybackState::Loading;
        playback.stop();
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(sender.send(Err("late failure".into())).is_err());
        playback.poll();
        assert_eq!(playback.state(), PlaybackState::Stopped);
        assert!(playback.take_error().is_none());
        assert!(playback.set_gain_db(f64::NAN).is_err());
        playback.set_volume(f32::NAN);
        assert_eq!(playback.volume, 0.0);
    }

    #[test]
    fn rendered_audio_matches_individual_and_album_predictions_with_monitor_attenuation() {
        let directory = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let paths = [0.6_f64, 0.05]
            .into_iter()
            .enumerate()
            .map(|(index, amplitude)| {
                let path = directory.path().join(format!("track-{index}.wav"));
                let spec = hound::WavSpec {
                    channels: 2,
                    sample_rate: 44_100,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                };
                let mut writer = hound::WavWriter::create(&path, spec).unwrap();
                for frame in 0..132_300 {
                    let sample = (std::f64::consts::TAU * 1000.0 * frame as f64 / 44_100.0).sin()
                        * amplitude;
                    writer.write_sample(sample as f32).unwrap();
                    writer.write_sample(sample as f32).unwrap();
                }
                writer.finalize().unwrap();
                path
            })
            .collect::<Vec<_>>();
        let analysis = crate::audio::analyze_files(&paths, |_, _| {}, &cancelled);
        let album = analysis.album.unwrap();
        let album_gain =
            crate::policy::normal_gain(album.integrated_lufs, album.true_peak_dbtp).unwrap();
        for (path, measurement) in paths.iter().zip(analysis.tracks) {
            let measurement = measurement.unwrap();
            let track_gain =
                crate::policy::normal_gain(measurement.integrated_lufs, measurement.true_peak_dbtp)
                    .unwrap();
            for gain in [track_gain, album_gain] {
                for monitor in [1.0_f32, 0.25] {
                    // Exercise the exact production cache, rate conversion, and
                    // gain source, then measure the rendered signal independently.
                    let pcm =
                        prepare_pcm(path, &file_stamp(path).unwrap(), 48_000, &cancelled).unwrap();
                    let rendered =
                        source(pcm, linear_gain(gain).unwrap(), monitor).collect::<Vec<_>>();
                    let mut meter = ebur128::EbuR128::new(2, 48_000, ebur128::Mode::I).unwrap();
                    meter.add_frames_f32(&rendered).unwrap();
                    let actual = meter.loudness_global().unwrap();
                    let predicted = measurement.integrated_lufs.unwrap()
                        + gain
                        + 20.0 * (monitor as f64).log10();
                    assert!(
                        (actual - predicted).abs() < 0.01,
                        "actual {actual}, predicted {predicted}"
                    );
                }
            }
        }
    }

    #[test]
    fn paused_track_does_not_auto_advance_when_its_final_buffer_is_already_queued() {
        let mut playback = Playback::new();
        let (player, _queue) = Player::new();
        playback.player = Some(player);
        playback.state = PlaybackState::Paused;
        playback.poll();
        assert_eq!(playback.state(), PlaybackState::Paused);
        playback.resume();
        playback.poll();
        assert_eq!(playback.state(), PlaybackState::Finished);
    }

    #[test]
    fn real_output_mixer_preserves_stereo_and_duplicates_mono_without_extra_gain() {
        for channels in [1, 2] {
            let (mixer, output) = rodio::mixer::mixer(rodio::nz!(2), rodio::nz!(48_000));
            mixer.add(source(
                cache(&[0.1, -0.2, 0.3, -0.4], 48_000, channels),
                2.0,
                0.25,
            ));
            let expected = if channels == 1 {
                vec![0.05, 0.05, -0.1, -0.1, 0.15, 0.15, -0.2, -0.2]
            } else {
                vec![0.05, -0.1, 0.15, -0.2]
            };
            assert_eq!(output.collect::<Vec<_>>(), expected);
        }
    }
}
