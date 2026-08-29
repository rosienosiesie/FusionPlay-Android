use aes::Aes128;
use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::NoPadding};
use anyhow::{Context, Result, bail};
#[cfg(not(target_os = "android"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(not(target_os = "android"))]
use cpal::{FromSample, SizedSample};
use hmac::{Hmac, Mac};
#[cfg(target_os = "android")]
use oboe::{
    AudioOutputCallback, AudioOutputStreamSafe, AudioStream, AudioStreamBase, AudioStreamBuilder,
    ContentType, DataCallbackResult, Error as OboeError, PerformanceMode,
    SampleRateConversionQuality, SharingMode, Stereo, Usage,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
#[cfg(target_os = "android")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};
use symphonia::core::audio::{Channels, Position};
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, well_known::CODEC_ID_AAC,
};
use symphonia::core::packet::Packet;
use symphonia::core::units::{Duration, Timestamp};
use symphonia::default::codecs::AacDecoder;

pub type EventEmitter = Arc<dyn Fn(Value) + Send + Sync + 'static>;

const POSITION_RESUME_SETTLING_WINDOW: StdDuration = StdDuration::from_millis(1_500);

/**
 * Shared playback gate for the control and RTSP/media paths.
 *
 * HyperOS can leave one or two already-sampled position frames queued when a
 * pause is applied. Treating the first advancing frame as an immediate resume
 * races the pause and re-opens audio output. The short settling window keeps
 * those stale frames from cancelling a real pause while explicit PLAY/RESUME
 * frames still open the gate immediately.
 */
#[derive(Clone, Debug)]
pub(crate) struct PlaybackGate {
    paused: Arc<AtomicBool>,
    source_paused: Arc<AtomicBool>,
    output_suspended: Arc<AtomicBool>,
    position_resume_not_before: Arc<Mutex<Option<Instant>>>,
}

impl PlaybackGate {
    pub(crate) fn with_output_suspension(output_suspended: Arc<AtomicBool>) -> Self {
        let suspended = output_suspended.load(Ordering::Acquire);
        Self {
            paused: Arc::new(AtomicBool::new(suspended)),
            source_paused: Arc::new(AtomicBool::new(false)),
            output_suspended,
            position_resume_not_before: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub(crate) fn is_source_paused(&self) -> bool {
        self.source_paused.load(Ordering::Acquire)
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.source_paused.store(paused, Ordering::Release);
        self.refresh_effective_pause();
        if let Ok(mut resume_not_before) = self.position_resume_not_before.lock() {
            *resume_not_before = paused.then(|| Instant::now() + POSITION_RESUME_SETTLING_WINDOW);
        }
    }

    pub(crate) fn set_output_suspended(&self, suspended: bool) {
        self.output_suspended.store(suspended, Ordering::Release);
        self.refresh_effective_pause();
    }

    pub(crate) fn paused_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.paused)
    }

    pub(crate) fn accepts_weak_resume(&self) -> bool {
        if self.output_suspended.load(Ordering::Acquire) {
            return false;
        }
        if !self.is_source_paused() {
            return true;
        }
        let Ok(mut resume_not_before) = self.position_resume_not_before.lock() else {
            return false;
        };
        if resume_not_before.is_some_and(|deadline| Instant::now() < deadline) {
            return false;
        }
        *resume_not_before = None;
        let resumed = self
            .source_paused
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if resumed {
            self.refresh_effective_pause();
        }
        resumed
    }

    fn refresh_effective_pause(&self) {
        self.paused.store(
            self.source_paused.load(Ordering::Acquire)
                || self.output_suspended.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
}

#[derive(Debug)]
struct RemoteChannelClosed {
    channel: &'static str,
    bytes_read: usize,
    bytes_expected: usize,
}

impl RemoteChannelClosed {
    const fn new(channel: &'static str, bytes_read: usize, bytes_expected: usize) -> Self {
        Self {
            channel,
            bytes_read,
            bytes_expected,
        }
    }
}

impl fmt::Display for RemoteChannelClosed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MiPlay {} source closed the channel after {}/{} bytes",
            self.channel, self.bytes_read, self.bytes_expected,
        )
    }
}

impl std::error::Error for RemoteChannelClosed {}

fn emit_normal_channel_close(
    events: &EventEmitter,
    error: &anyhow::Error,
    generation: u64,
) -> bool {
    let Some(closed) = error.downcast_ref::<RemoteChannelClosed>() else {
        return false;
    };
    events(json!({
        "event": "media_channel_closed",
        "protocol": "xiaomi_miplay",
        "outcome": "normal",
        "channel": closed.channel,
        "bytes_read": closed.bytes_read,
        "bytes_expected": closed.bytes_expected,
        "media_generation": generation,
    }));
    true
}

#[derive(Clone, Debug)]
pub struct StreamKeys {
    pub auth_key: [u8; 16],
    pub stream_key: [u8; 16],
    pub stream_iv: [u8; 16],
}

impl StreamKeys {
    pub fn from_strings(auth_key: &str, stream_key: &str, stream_iv: &str) -> Result<Self> {
        Ok(Self {
            auth_key: exact_ascii_key(auth_key, "RTSP auth key")?,
            stream_key: exact_ascii_key(stream_key, "media AES key")?,
            stream_iv: exact_ascii_key(stream_iv, "media AES IV")?,
        })
    }

    pub(crate) fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.stream_key);
        digest.update(self.stream_iv);
        hex::encode(&digest.finalize()[..8])
    }
}

fn exact_ascii_key(value: &str, description: &str) -> Result<[u8; 16]> {
    let bytes = value.as_bytes();
    if bytes.len() != 16 || !bytes.is_ascii() {
        bail!("{description} must contain exactly 16 ASCII bytes");
    }
    let mut key = [0_u8; 16];
    key.copy_from_slice(bytes);
    Ok(key)
}

/// Sender-clock mapping used by Xiaomi's grouped-audio mode.
///
/// HyperOS advertises a UDP timer server in the RTSP OPTIONS request and then
/// sends a `TimeOffset` value. PES presentation timestamps plus that offset
/// are expressed on the synchronized timer-server clock.
struct MiPlayPresentationClock {
    local_epoch: Instant,
    clock_model_revision: AtomicU64,
    clock_base_local_micros: AtomicI64,
    clock_offset_at_base_micros: AtomicI64,
    clock_rate_ppb: AtomicI64,
    server_clock_valid: AtomicBool,
    presentation_offset_micros: AtomicI64,
    presentation_offset_valid: AtomicBool,
}

/// Live receiver measurements mirrored back to Xiaomi's RTSP source.
///
/// The official sink reports the current playout queue, measured output
/// latency, received RTP packet number, and rolling bitrate roughly four times
/// per second. RTP arrival samples are also retained in the sender's 90 kHz
/// clock domain so network jitter can be diagnosed independently from the
/// playout queue.
struct MiPlayLatencyTelemetry {
    rtp: Mutex<RtpTelemetryState>,
    buffered_frames: AtomicU64,
    output_latency_micros: AtomicU64,
}

struct RtpTelemetryState {
    first_arrival: Option<Instant>,
    last_sequence: Option<u64>,
    last_timestamp: Option<u64>,
    regression_samples: VecDeque<(f64, f64)>,
    bitrate_samples: VecDeque<(Instant, usize)>,
    bitrate_bytes: usize,
    bitrate_bps: u64,
    arrival_residual_millis: f64,
}

#[derive(Clone, Copy, Debug)]
struct MiPlayLatencySnapshot {
    latency_millis: u32,
    bitrate_bps: u64,
    rtp_packet_number: u64,
    arrival_residual_millis: f64,
}

impl MiPlayLatencyTelemetry {
    fn new() -> Self {
        Self {
            rtp: Mutex::new(RtpTelemetryState {
                first_arrival: None,
                last_sequence: None,
                last_timestamp: None,
                regression_samples: VecDeque::with_capacity(MIPLAY_RTP_REGRESSION_SAMPLES),
                bitrate_samples: VecDeque::new(),
                bitrate_bytes: 0,
                bitrate_bps: 0,
                arrival_residual_millis: 0.0,
            }),
            buffered_frames: AtomicU64::new(0),
            output_latency_micros: AtomicU64::new(0),
        }
    }

    fn observe_rtp(
        &self,
        sequence_number: u16,
        timestamp: u32,
        packet_bytes: usize,
        arrival: Instant,
    ) {
        let Ok(mut state) = self.rtp.lock() else {
            return;
        };
        let sequence = extend_wrapping_counter(state.last_sequence, u64::from(sequence_number), 16);
        let timestamp = extend_wrapping_counter(state.last_timestamp, u64::from(timestamp), 32);
        state.last_sequence = Some(
            state
                .last_sequence
                .map_or(sequence, |last| last.max(sequence)),
        );
        state.last_timestamp = Some(timestamp);

        let first_arrival = *state.first_arrival.get_or_insert(arrival);
        let arrival_ticks = arrival
            .saturating_duration_since(first_arrival)
            .as_secs_f64()
            * f64::from(MIPLAY_RTP_CLOCK_HZ);
        state
            .regression_samples
            .push_back((timestamp as f64, arrival_ticks));
        while state.regression_samples.len() > MIPLAY_RTP_REGRESSION_SAMPLES {
            state.regression_samples.pop_front();
        }
        state.arrival_residual_millis =
            rtp_arrival_residual_millis(&state.regression_samples).unwrap_or(0.0);

        state.bitrate_samples.push_back((arrival, packet_bytes));
        state.bitrate_bytes = state.bitrate_bytes.saturating_add(packet_bytes);
        while state
            .bitrate_samples
            .front()
            .is_some_and(|(sampled_at, _)| {
                arrival.saturating_duration_since(*sampled_at) > MIPLAY_BITRATE_WINDOW
            })
        {
            if let Some((_, bytes)) = state.bitrate_samples.pop_front() {
                state.bitrate_bytes = state.bitrate_bytes.saturating_sub(bytes);
            }
        }
        if let Some((oldest, _)) = state.bitrate_samples.front() {
            let elapsed = arrival.saturating_duration_since(*oldest).as_secs_f64();
            if elapsed >= MIPLAY_MINIMUM_BITRATE_WINDOW.as_secs_f64() {
                state.bitrate_bps = (state.bitrate_bytes as f64 * 8.0 / elapsed)
                    .round()
                    .max(0.0) as u64;
            }
        }
    }

    fn update_buffered_frames(&self, buffered_frames: usize) {
        self.buffered_frames.store(
            u64::try_from(buffered_frames).unwrap_or(u64::MAX),
            Ordering::Release,
        );
    }

    fn update_output_latency(&self, output_latency_millis: f64) {
        let output_latency_micros = if output_latency_millis.is_finite() {
            (output_latency_millis.max(0.0) * 1_000.0).round() as u64
        } else {
            0
        };
        self.output_latency_micros
            .store(output_latency_micros, Ordering::Release);
    }

    fn snapshot(&self) -> Option<MiPlayLatencySnapshot> {
        let state = self.rtp.lock().ok()?;
        let rtp_packet_number = state.last_sequence?;
        let queue_micros = self
            .buffered_frames
            .load(Ordering::Acquire)
            .saturating_mul(1_000_000)
            / u64::from(MIPLAY_SAMPLE_RATE_HZ);
        let latency_micros =
            queue_micros.saturating_add(self.output_latency_micros.load(Ordering::Acquire));
        Some(MiPlayLatencySnapshot {
            latency_millis: u32::try_from(latency_micros.saturating_add(500) / 1_000)
                .unwrap_or(u32::MAX),
            bitrate_bps: state.bitrate_bps,
            rtp_packet_number,
            arrival_residual_millis: state.arrival_residual_millis,
        })
    }
}

fn extend_wrapping_counter(previous: Option<u64>, raw: u64, bits: u32) -> u64 {
    let Some(previous) = previous else {
        return raw;
    };
    let modulus = 1_u64 << bits;
    let mask = modulus - 1;
    let half = modulus / 2;
    let base = previous & !mask;
    let mut candidate = base | (raw & mask);
    if candidate.saturating_add(half) < previous {
        candidate = candidate.saturating_add(modulus);
    } else if candidate > previous.saturating_add(half) && candidate >= modulus {
        candidate -= modulus;
    }
    candidate
}

fn rtp_arrival_residual_millis(samples: &VecDeque<(f64, f64)>) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let count = samples.len() as f64;
    let mean_timestamp = samples.iter().map(|sample| sample.0).sum::<f64>() / count;
    let mean_arrival = samples.iter().map(|sample| sample.1).sum::<f64>() / count;
    let (covariance, variance) = samples.iter().fold((0.0, 0.0), |acc, sample| {
        let timestamp = sample.0 - mean_timestamp;
        let arrival = sample.1 - mean_arrival;
        (acc.0 + timestamp * arrival, acc.1 + timestamp * timestamp)
    });
    if !variance.is_finite() || variance <= f64::EPSILON {
        return None;
    }
    let slope = covariance / variance;
    let intercept = mean_arrival - slope * mean_timestamp;
    let latest = samples.back()?;
    let residual_ticks = latest.1 - (slope * latest.0 + intercept);
    Some(residual_ticks / (f64::from(MIPLAY_RTP_CLOCK_HZ) / 1_000.0))
}

#[derive(Clone, Copy, Debug)]
struct TimerSample {
    local_midpoint_micros: i64,
    offset_micros: i64,
    round_trip_micros: i64,
}

#[derive(Clone, Copy, Debug)]
struct ClockFitPoint {
    local_micros: i64,
    offset_micros: i64,
}

#[derive(Clone, Copy, Debug)]
struct ClockEstimate {
    base_local_micros: i64,
    offset_at_base_micros: f64,
    frequency_ratio: f64,
    minimum_round_trip_micros: i64,
    filtered_sample_count: usize,
    history_sample_count: usize,
}

#[derive(Clone, Copy, Debug)]
struct AppliedClockModel {
    offset_micros: i64,
    target_offset_micros: i64,
    phase_error_micros: i64,
    frequency_ppm: f64,
}

struct TimerClockEstimator {
    raw_samples: VecDeque<TimerSample>,
    history: VecDeque<ClockFitPoint>,
}

impl TimerClockEstimator {
    fn new() -> Self {
        Self {
            raw_samples: VecDeque::with_capacity(MIPLAY_CLOCK_FILTER_SAMPLES),
            history: VecDeque::with_capacity(MIPLAY_CLOCK_FIT_HISTORY),
        }
    }

    fn observe_round(&mut self, round_samples: &[TimerSample]) -> Option<ClockEstimate> {
        if round_samples.is_empty() {
            return None;
        }
        for sample in round_samples {
            if self.raw_samples.len() == MIPLAY_CLOCK_FILTER_SAMPLES {
                self.raw_samples.pop_front();
            }
            self.raw_samples.push_back(*sample);
        }

        let filtered_offset =
            trimmed_clock_offset(self.raw_samples.iter().map(|sample| sample.offset_micros))?;
        let best_sample = round_samples
            .iter()
            .min_by_key(|sample| sample.round_trip_micros)?;
        self.history.push_back(ClockFitPoint {
            local_micros: best_sample.local_midpoint_micros,
            offset_micros: filtered_offset,
        });
        while self.history.len() > MIPLAY_CLOCK_FIT_HISTORY {
            self.history.pop_front();
        }

        let (base_local_micros, offset_at_base_micros, frequency_ratio) =
            fit_clock_line(&self.history);
        Some(ClockEstimate {
            base_local_micros,
            offset_at_base_micros,
            frequency_ratio,
            minimum_round_trip_micros: best_sample.round_trip_micros,
            filtered_sample_count: self.raw_samples.len(),
            history_sample_count: self.history.len(),
        })
    }
}

fn trimmed_clock_offset(samples: impl Iterator<Item = i64>) -> Option<i64> {
    let mut samples = samples.collect::<Vec<_>>();
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let keep = if samples.len() < 6 {
        samples.len().saturating_sub(1).max(1)
    } else {
        samples.len() / 2
    };
    let start = (samples.len() - keep) / 2;
    let sum = samples[start..start + keep]
        .iter()
        .fold(0_i128, |sum, sample| sum + i128::from(*sample));
    Some((sum / keep as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
}

fn fit_clock_line(history: &VecDeque<ClockFitPoint>) -> (i64, f64, f64) {
    let latest = history.back().copied().unwrap_or(ClockFitPoint {
        local_micros: 0,
        offset_micros: 0,
    });
    let span_micros = history
        .front()
        .map(|first| latest.local_micros.saturating_sub(first.local_micros))
        .unwrap_or(0);
    if history.len() < MIPLAY_CLOCK_MIN_FIT_ROUNDS || span_micros < MIPLAY_CLOCK_MIN_FIT_SPAN_MICROS
    {
        return (latest.local_micros, latest.offset_micros as f64, 1.0);
    }

    let count = history.len() as f64;
    let mean_x = history
        .iter()
        .map(|point| (point.local_micros - latest.local_micros) as f64)
        .sum::<f64>()
        / count;
    let mean_y = history
        .iter()
        .map(|point| point.offset_micros as f64)
        .sum::<f64>()
        / count;
    let (covariance, variance) = history.iter().fold((0.0, 0.0), |acc, point| {
        let x = (point.local_micros - latest.local_micros) as f64 - mean_x;
        let y = point.offset_micros as f64 - mean_y;
        (acc.0 + x * y, acc.1 + x * x)
    });
    if !variance.is_finite() || variance <= f64::EPSILON {
        return (latest.local_micros, latest.offset_micros as f64, 1.0);
    }
    let offset_slope = (covariance / variance).clamp(
        -MIPLAY_CLOCK_MAX_FREQUENCY_ERROR,
        MIPLAY_CLOCK_MAX_FREQUENCY_ERROR,
    );
    let offset_at_base = mean_y - offset_slope * mean_x;
    (latest.local_micros, offset_at_base, 1.0 + offset_slope)
}

fn smooth_clock_model(
    initialized: bool,
    current_offset_micros: i64,
    current_rate_ppb: i64,
    target_offset_micros: i64,
    target_rate_ppb: i64,
) -> (i64, i64, i64) {
    let phase_error = target_offset_micros.saturating_sub(current_offset_micros);
    if !initialized {
        return (target_offset_micros, target_rate_ppb, phase_error);
    }
    let phase_step = ((phase_error as f64 * MIPLAY_CLOCK_PHASE_SMOOTHING).round() as i64).clamp(
        -MIPLAY_CLOCK_MAX_PHASE_STEP_MICROS,
        MIPLAY_CLOCK_MAX_PHASE_STEP_MICROS,
    );
    let rate_step = ((target_rate_ppb.saturating_sub(current_rate_ppb)) as f64
        * MIPLAY_CLOCK_RATE_SMOOTHING)
        .round() as i64;
    (
        current_offset_micros.saturating_add(phase_step),
        current_rate_ppb.saturating_add(rate_step),
        phase_error,
    )
}

struct SessionRunFlag(Arc<AtomicBool>);

impl Drop for SessionRunFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl MiPlayPresentationClock {
    fn new() -> Self {
        Self {
            local_epoch: Instant::now(),
            clock_model_revision: AtomicU64::new(0),
            clock_base_local_micros: AtomicI64::new(0),
            clock_offset_at_base_micros: AtomicI64::new(0),
            clock_rate_ppb: AtomicI64::new(0),
            server_clock_valid: AtomicBool::new(false),
            presentation_offset_micros: AtomicI64::new(0),
            presentation_offset_valid: AtomicBool::new(false),
        }
    }

    fn local_now_micros(&self) -> i64 {
        self.local_epoch.elapsed().as_micros().min(i64::MAX as u128) as i64
    }

    fn synchronized_server_now_micros(&self) -> Option<i64> {
        if !self.server_clock_valid.load(Ordering::Acquire) {
            return None;
        }
        let local_now = self.local_now_micros();
        let (base_local, offset_at_base, rate_ppb) = self.clock_model_snapshot();
        let elapsed = local_now.saturating_sub(base_local);
        let frequency_adjustment = (i128::from(elapsed) * i128::from(rate_ppb) / 1_000_000_000_i128)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        Some(
            local_now
                .saturating_add(offset_at_base)
                .saturating_add(frequency_adjustment),
        )
    }

    fn clock_model_snapshot(&self) -> (i64, i64, i64) {
        loop {
            let before = self.clock_model_revision.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let snapshot = (
                self.clock_base_local_micros.load(Ordering::Relaxed),
                self.clock_offset_at_base_micros.load(Ordering::Relaxed),
                self.clock_rate_ppb.load(Ordering::Relaxed),
            );
            let after = self.clock_model_revision.load(Ordering::Acquire);
            if before == after {
                return snapshot;
            }
        }
    }

    fn update_server_clock(&self, estimate: ClockEstimate) -> AppliedClockModel {
        let local_now = self.local_now_micros();
        let target_rate_ppb = ((estimate.frequency_ratio - 1.0) * 1_000_000_000.0)
            .round()
            .clamp(i64::MIN as f64, i64::MAX as f64) as i64;
        let target_offset_now = (estimate.offset_at_base_micros
            + (local_now.saturating_sub(estimate.base_local_micros)) as f64
                * (estimate.frequency_ratio - 1.0))
            .round()
            .clamp(i64::MIN as f64, i64::MAX as f64) as i64;
        let initialized = self.server_clock_valid.load(Ordering::Acquire);
        let (current_base, current_offset_at_base, current_rate_ppb) = self.clock_model_snapshot();
        let current_offset_now = if initialized {
            let elapsed = local_now.saturating_sub(current_base);
            let adjustment = (i128::from(elapsed) * i128::from(current_rate_ppb)
                / 1_000_000_000_i128)
                .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            current_offset_at_base.saturating_add(adjustment)
        } else {
            0
        };
        let (applied_offset, applied_rate_ppb, phase_error) = smooth_clock_model(
            initialized,
            current_offset_now,
            current_rate_ppb,
            target_offset_now,
            target_rate_ppb,
        );

        self.clock_model_revision.fetch_add(1, Ordering::AcqRel);
        self.clock_base_local_micros
            .store(local_now, Ordering::Relaxed);
        self.clock_offset_at_base_micros
            .store(applied_offset, Ordering::Relaxed);
        self.clock_rate_ppb
            .store(applied_rate_ppb, Ordering::Relaxed);
        self.clock_model_revision.fetch_add(1, Ordering::Release);
        self.server_clock_valid.store(true, Ordering::Release);

        AppliedClockModel {
            offset_micros: applied_offset,
            target_offset_micros: target_offset_now,
            phase_error_micros: phase_error,
            frequency_ppm: applied_rate_ppb as f64 / 1_000.0,
        }
    }

    fn set_presentation_offset(&self, offset_micros: i64) {
        self.presentation_offset_micros
            .store(offset_micros, Ordering::Release);
        self.presentation_offset_valid
            .store(true, Ordering::Release);
    }

    fn synchronization_ready(&self) -> bool {
        self.server_clock_valid.load(Ordering::Acquire)
            && self.presentation_offset_valid.load(Ordering::Acquire)
    }
}

fn parse_timer_server(value: &str, fallback_host: Ipv4Addr) -> Option<SocketAddr> {
    let (host, port) = value.trim().split_once(':')?;
    let port = port.trim().parse::<u16>().ok()?;
    let advertised = host
        .trim()
        .parse::<u32>()
        .ok()
        .map(Ipv4Addr::from)
        .filter(|address| !address.is_unspecified())
        .unwrap_or(fallback_host);
    Some(SocketAddr::from((advertised, port)))
}

fn spawn_timer_synchronizer(
    remote: SocketAddr,
    clock: Arc<MiPlayPresentationClock>,
    paused: Arc<AtomicBool>,
    force_sync: Arc<AtomicBool>,
    media_generation: u64,
    current_media_generation: Arc<AtomicU64>,
    session_running: Arc<AtomicBool>,
    events: EventEmitter,
) {
    let _ = thread::Builder::new()
        .name("miplay-clock-sync".to_owned())
        .spawn(move || {
            let socket = match UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) {
                Ok(socket) => socket,
                Err(error) => {
                    events(json!({
                        "event": "error",
                        "code": "miplay_clock_sync_bind_failed",
                        "message": format!("小米妙播同步时钟初始化失败：{error}"),
                    }));
                    return;
                }
            };
            socket
                .set_read_timeout(Some(StdDuration::from_millis(350)))
                .ok();
            socket
                .set_write_timeout(Some(StdDuration::from_millis(350)))
                .ok();
            if let Err(error) = socket.connect(remote) {
                events(json!({
                    "event": "error",
                    "code": "miplay_clock_sync_connect_failed",
                    "message": format!("小米妙播同步时钟连接失败：{error}"),
                }));
                return;
            }

            let mut sequence = 0_u32;
            let mut estimator = TimerClockEstimator::new();
            while session_running.load(Ordering::Acquire)
                && current_media_generation.load(Ordering::Acquire) == media_generation
            {
                if paused.load(Ordering::Acquire) {
                    thread::sleep(StdDuration::from_millis(100));
                    continue;
                }
                let mut round_samples = Vec::with_capacity(MIPLAY_CLOCK_SYNC_SAMPLES);
                let mut round_interrupted = false;
                for _ in 0..MIPLAY_CLOCK_SYNC_SAMPLES {
                    if !session_running.load(Ordering::Acquire)
                        || current_media_generation.load(Ordering::Acquire) != media_generation
                    {
                        return;
                    }
                    if paused.load(Ordering::Acquire) {
                        round_interrupted = true;
                        break;
                    }
                    sequence = sequence.wrapping_add(1);
                    let mut packet = [0_u8; MIPLAY_TIMER_PACKET_BYTES];
                    let client_send = clock.local_now_micros();
                    packet[0..8].copy_from_slice(&client_send.to_le_bytes());
                    packet[32..36].copy_from_slice(&sequence.to_le_bytes());

                    if socket.send(&packet).ok() != Some(packet.len()) {
                        continue;
                    }
                    let mut response = [0_u8; MIPLAY_TIMER_PACKET_BYTES];
                    let Ok(received) = socket.recv(&mut response) else {
                        continue;
                    };
                    let client_receive = clock.local_now_micros();
                    if received != response.len() {
                        continue;
                    }
                    let Some((offset, network_rtt)) =
                        decode_timer_sample(&response, sequence, client_send, client_receive)
                    else {
                        continue;
                    };
                    let local_midpoint_micros =
                        client_send.saturating_add(client_receive.saturating_sub(client_send) / 2);
                    round_samples.push(TimerSample {
                        local_midpoint_micros,
                        offset_micros: offset,
                        round_trip_micros: network_rtt,
                    });
                    thread::sleep(StdDuration::from_millis(
                        MIPLAY_CLOCK_SAMPLE_INTERVAL_MILLIS,
                    ));
                }

                if round_interrupted {
                    continue;
                }
                if let Some(estimate) = estimator.observe_round(&round_samples) {
                    let applied = clock.update_server_clock(estimate);
                    events(json!({
                        "event": "clock_synchronized",
                        "protocol": "xiaomi_miplay",
                        "timer_server": remote.to_string(),
                        "server_minus_local_us": applied.offset_micros,
                        "target_server_minus_local_us": applied.target_offset_micros,
                        "phase_error_us": applied.phase_error_micros,
                        "frequency_ppm": applied.frequency_ppm,
                        "round_trip_us": estimate.minimum_round_trip_micros,
                        "filtered_samples": estimate.filtered_sample_count,
                        "fit_history_samples": estimate.history_sample_count,
                    }));
                }

                for _ in 0..MIPLAY_CLOCK_RESYNC_POLLS {
                    if !session_running.load(Ordering::Acquire)
                        || current_media_generation.load(Ordering::Acquire) != media_generation
                    {
                        return;
                    }
                    if force_sync.swap(false, Ordering::AcqRel) {
                        break;
                    }
                    thread::sleep(StdDuration::from_millis(MIPLAY_CLOCK_FORCE_POLL_MILLIS));
                }
            }
        });
}

fn decode_timer_sample(
    response: &[u8],
    expected_sequence: u32,
    client_send: i64,
    client_receive: i64,
) -> Option<(i64, i64)> {
    if response.len() != MIPLAY_TIMER_PACKET_BYTES
        || u32::from_le_bytes(response[32..36].try_into().ok()?) != expected_sequence
    {
        return None;
    }
    let server_send = i64::from_le_bytes(response[16..24].try_into().ok()?);
    let server_receive = i64::from_le_bytes(response[24..32].try_into().ok()?);
    let network_rtt = client_receive
        .saturating_sub(client_send)
        .saturating_sub(server_send.saturating_sub(server_receive));
    if !(0..=MIPLAY_CLOCK_MAX_RTT_MICROS).contains(&network_rtt) {
        return None;
    }
    let offset = server_receive
        .saturating_sub(client_send)
        .saturating_add(server_send.saturating_sub(client_receive))
        / 2;
    Some((offset, network_rtt))
}

pub(crate) fn spawn_rtsp_receiver(
    host: Ipv4Addr,
    port: u16,
    keys: Option<StreamKeys>,
    output_device: Option<String>,
    playback_gate: PlaybackGate,
    volume_percent: Arc<AtomicU32>,
    media_generation: u64,
    current_media_generation: Arc<AtomicU64>,
    events: EventEmitter,
) {
    let _ = thread::Builder::new()
        .name("miplay-rtsp".to_owned())
        .spawn(move || {
            if let Err(error) = run_rtsp_receiver(
                host,
                port,
                keys,
                output_device,
                playback_gate,
                volume_percent,
                media_generation,
                Arc::clone(&current_media_generation),
                Arc::clone(&events),
            ) {
                if current_media_generation.load(Ordering::Acquire) != media_generation {
                    events(json!({
                        "event": "media_session_replaced",
                        "protocol": "xiaomi_miplay",
                        "media_generation": media_generation,
                    }));
                    return;
                }
                if emit_normal_channel_close(&events, &error, media_generation) {
                    // A normal RTSP EOF is a media-channel boundary, not a
                    // control-session disconnect. HyperOS commonly closes
                    // this channel while switching tracks and then keeps
                    // sending metadata/control events on the same session.
                    // Marking the whole session inactive here makes the next
                    // title and artwork remain cached forever.
                    return;
                }
                events(json!({
                    "event": "error",
                    "code": "miplay_rtsp_failed",
                    "media_generation": media_generation,
                    "message": format!("小米妙播媒体通道失败：{error:#}"),
                }));
                events(json!({
                    "event": "playback_state",
                    "raw_state": 4,
                    "playing": false,
                    "session_active": false,
                }));
            }
        });
}

fn run_rtsp_receiver(
    host: Ipv4Addr,
    port: u16,
    keys: Option<StreamKeys>,
    output_device: Option<String>,
    playback_gate: PlaybackGate,
    volume_percent: Arc<AtomicU32>,
    media_generation: u64,
    current_media_generation: Arc<AtomicU64>,
    events: EventEmitter,
) -> Result<()> {
    let remote = SocketAddr::from((host, port));
    let mut control = TcpStream::connect_timeout(&remote, StdDuration::from_secs(5))
        .with_context(|| format!("connect RTSP source {host}:{port}"))?;
    control.set_nodelay(true).ok();
    control
        .set_read_timeout(Some(MIPLAY_RTSP_POLL_INTERVAL))
        .ok();
    control
        .set_write_timeout(Some(StdDuration::from_secs(3)))
        .ok();

    events(json!({
        "event": "status",
        "state": "media_connecting",
        "message": format!("已连接小米妙播 RTSP 源 {host}:{port}"),
    }));

    let mut input = Vec::new();
    let mut sent_options = false;
    let mut data_ports: Option<(u16, u16)> = None;
    let mut presentation_url = format!("rtsp://{host}/wfd1.0/streamid=0");
    let mut session_id = String::new();
    let local_challenge = random_hex_32();
    let presentation_clock = Arc::new(MiPlayPresentationClock::new());
    let latency_telemetry = Arc::new(MiPlayLatencyTelemetry::new());
    let mut latency_cseq = 1_000_u32;
    let mut next_latency_report = Instant::now() + MIPLAY_LATENCY_REPORT_INTERVAL;
    let mut latency_reporting_started = false;
    let timer_sync_running = Arc::new(AtomicBool::new(true));
    let _timer_sync_guard = SessionRunFlag(Arc::clone(&timer_sync_running));
    let timer_force_sync = Arc::new(AtomicBool::new(false));
    let mut timer_sync_started = false;

    loop {
        if current_media_generation.load(Ordering::Acquire) != media_generation {
            return Ok(());
        }
        let now = Instant::now();
        if !session_id.is_empty() && now >= next_latency_report {
            if let Some(snapshot) = latency_telemetry.snapshot() {
                send_video_latency_request(&mut control, latency_cseq, snapshot)?;
                latency_cseq = latency_cseq.wrapping_add(1);
                if !latency_reporting_started {
                    latency_reporting_started = true;
                    events(json!({
                        "event": "latency_feedback_started",
                        "protocol": "xiaomi_miplay",
                        "interval_ms": MIPLAY_LATENCY_REPORT_INTERVAL.as_millis(),
                        "rtp_clock_hz": MIPLAY_RTP_CLOCK_HZ,
                        "latency_ms": snapshot.latency_millis,
                        "bitrate_bps": snapshot.bitrate_bps,
                        "rtp_packet_number": snapshot.rtp_packet_number,
                        "arrival_residual_ms": snapshot.arrival_residual_millis,
                    }));
                }
            }
            next_latency_report = now + MIPLAY_LATENCY_REPORT_INTERVAL;
        }
        let Some(message) = read_rtsp_message(&mut control, &mut input)? else {
            continue;
        };
        let cseq = message.header("cseq").unwrap_or("0").to_owned();
        if message.is_response() {
            if cseq == "2" && message.start_line.contains("200") {
                if let Some(session) = message.header("session") {
                    session_id = session
                        .split(';')
                        .next()
                        .unwrap_or(session)
                        .trim()
                        .to_owned();
                }
                let request = format!(
                    "PLAY {presentation_url} RTSP/1.0\r\n\
                     User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
                     CSeq: 3\r\n\
                     Session: {session_id}\r\n\r\n"
                );
                control.write_all(request.as_bytes())?;
            } else if cseq == "3" && message.start_line.contains("200") {
                playback_gate.set_paused(false);
                events(json!({
                    "event": "stream_started",
                    "source_codec": "aac",
                    "source_sample_rate": MIPLAY_SAMPLE_RATE_HZ,
                    "source_channels": 2,
                    "source_bits": 16,
                    "transport": "rtp_mpegts_tcp",
                }));
                events(json!({
                    "event": "playback_state",
                    "raw_state": 2,
                    "playing": true,
                    "session_active": true,
                }));
            }
            continue;
        }

        let method = message
            .start_line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if !timer_sync_started {
            let timer_server = message
                .header("wfd_timer_server_port")
                .or_else(|| parameter_value(message.body_text(), "wfd_timer_server_port"))
                .and_then(|value| parse_timer_server(value, host));
            if let Some(timer_server) = timer_server {
                timer_sync_started = true;
                spawn_timer_synchronizer(
                    timer_server,
                    Arc::clone(&presentation_clock),
                    playback_gate.paused_flag(),
                    Arc::clone(&timer_force_sync),
                    media_generation,
                    Arc::clone(&current_media_generation),
                    Arc::clone(&timer_sync_running),
                    Arc::clone(&events),
                );
            }
        }
        match method.as_str() {
            "OPTIONS" => {
                let response = if let Some(keys) = keys.as_ref() {
                    let ack = message
                        .header("authmsg")
                        .map(|challenge| hmac_sha256_hex(&keys.auth_key, challenge.as_bytes()))
                        .unwrap_or_default();
                    format!(
                        "RTSP/1.0 200 OK\r\n\
                         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
                         CSeq: {cseq}\r\n\
                         Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER\r\n\
                         authKeyType:2\r\n\
                         authAlgorithmVal:4\r\n\
                         authMsgAck:{ack}\r\n\r\n"
                    )
                } else {
                    format!(
                        "RTSP/1.0 200 OK\r\n\
                         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
                         CSeq: {cseq}\r\n\
                         Public: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER\r\n\r\n"
                    )
                };
                control.write_all(response.as_bytes())?;
                if !sent_options {
                    sent_options = true;
                    let auth_header = keys
                        .as_ref()
                        .map(|_| format!("authMsg:{local_challenge}\r\n"))
                        .unwrap_or_default();
                    let request = format!(
                        "OPTIONS * RTSP/1.0\r\n\
                         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
                         CSeq: 1\r\n\
                         Require: org.wfa.wfd1.0\r\n\
                         lib_version: audio-display-release2.1 2.1.5071614\r\n\
                         {auth_header}\r\n"
                    );
                    control.write_all(request.as_bytes())?;
                }
            }
            "GET_PARAMETER" => {
                if message.body_text().contains("wfd_audio_codecs_v2") {
                    if data_ports.is_none() {
                        let image = TcpStream::connect_timeout(&remote, StdDuration::from_secs(5))
                            .context("connect MiPlay image data socket")?;
                        let multi = TcpStream::connect_timeout(&remote, StdDuration::from_secs(5))
                            .context("connect MiPlay audio data socket")?;
                        let image_port = image.local_addr()?.port();
                        let multi_port = multi.local_addr()?.port();
                        spawn_image_drain(image);
                        spawn_media_reader(
                            multi,
                            keys.clone(),
                            output_device.clone(),
                            playback_gate.paused_flag(),
                            Arc::clone(&volume_percent),
                            Arc::clone(&presentation_clock),
                            Arc::clone(&latency_telemetry),
                            media_generation,
                            Arc::clone(&current_media_generation),
                            Arc::clone(&events),
                        );
                        data_ports = Some((image_port, multi_port));
                    }
                    let body = concat!(
                        "wfd_audio_codecs_v2: 15 3 3\r\n",
                        "wfd_video_formats: none\r\n",
                        "wfd_video_enctype: none\r\n",
                        "wfd_video_gamuttype: none\r\n",
                        "wfd_video_bitrate: none\r\n",
                        "wfd_current_video_info: none\r\n",
                        "wfd_client_rtp_ports: RTP/AVP/TCP;interleaved mode=play\r\n",
                        "miplay_support_image: none\r\n",
                        "wfd_standby_resume_capability: supported\r\n",
                        "wfd_content_SP_protection: 4 1 256 3 1 1 0 0\r\n",
                        "wfd_support_secure_win:enable\r\n",
                        "device_info: -1 -1 -1 -1 -1 -1 -1\r\n"
                    );
                    send_rtsp_response(&mut control, &cseq, body)?;
                } else {
                    send_rtsp_response(&mut control, &cseq, "")?;
                }
            }
            "SET_PARAMETER" => {
                let body = message.body_text();
                if let Some(offset) = parameter_value(body, "wfd_timeoffset")
                    .and_then(|value| value.parse::<i64>().ok())
                {
                    presentation_clock.set_presentation_offset(offset);
                    events(json!({
                        "event": "presentation_clock_updated",
                        "protocol": "xiaomi_miplay",
                        "time_offset_us": offset,
                        "clock_synchronized": presentation_clock
                            .server_clock_valid
                            .load(Ordering::Acquire),
                    }));
                }
                if let Some(url) = body
                    .lines()
                    .find_map(|line| line.strip_prefix("wfd_presentation_URL:"))
                    .and_then(|line| line.split_whitespace().next())
                {
                    presentation_url = url.to_owned();
                }
                send_rtsp_response(&mut control, &cseq, "")?;
                if body.contains("wfd_trigger_method: SETUP") {
                    let (image_port, multi_port) = data_ports
                        .context("MiPlay requested SETUP before data sockets were negotiated")?;
                    let request = format!(
                        "SETUP {presentation_url} RTSP/1.0\r\n\
                         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
                         CSeq: 2\r\n\
                         Transport: RTP/AVP/TCP;interleaved=0-1\r\n\
                         MultiPort: image_port={image_port};multi_port={multi_port}\r\n\r\n"
                    );
                    control.write_all(request.as_bytes())?;
                }
            }
            "TIME_OFFSET" => {
                if let Some(offset) = message
                    .header("timeoffset")
                    .and_then(|value| value.parse::<i64>().ok())
                {
                    presentation_clock.set_presentation_offset(offset);
                    events(json!({
                        "event": "presentation_clock_updated",
                        "protocol": "xiaomi_miplay",
                        "time_offset_us": offset,
                        "clock_synchronized": presentation_clock
                            .server_clock_valid
                            .load(Ordering::Acquire),
                    }));
                }
                send_rtsp_response(&mut control, &cseq, "")?;
            }
            "VIDEO_LATENCY" => {
                send_rtsp_response(&mut control, &cseq, "")?;
            }
            "PAUSE" => {
                playback_gate.set_paused(true);
                send_rtsp_response(&mut control, &cseq, "")?;
                events(json!({
                    "event": "playback_state",
                    "raw_state": 3,
                    "playing": false,
                    "session_active": true,
                }));
            }
            "PLAY" => {
                timer_force_sync.store(true, Ordering::Release);
                playback_gate.set_paused(false);
                send_rtsp_response(&mut control, &cseq, "")?;
                events(json!({
                    "event": "playback_state",
                    "raw_state": 2,
                    "playing": true,
                    "session_active": true,
                }));
            }
            "TEARDOWN" => {
                send_rtsp_response(&mut control, &cseq, "")?;
                break;
            }
            _ => send_rtsp_response(&mut control, &cseq, "")?,
        }
    }

    if current_media_generation.load(Ordering::Acquire) == media_generation {
        events(json!({
            "event": "stream_stopped",
            "source": "xiaomi",
            "media_generation": media_generation,
        }));
    }
    Ok(())
}

fn spawn_image_drain(mut stream: TcpStream) {
    let _ = thread::Builder::new()
        .name("miplay-image-drain".to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; 8192];
            while stream.read(&mut buffer).is_ok_and(|read| read > 0) {}
        });
}

fn spawn_media_reader(
    stream: TcpStream,
    keys: Option<StreamKeys>,
    output_device: Option<String>,
    paused: Arc<AtomicBool>,
    volume_percent: Arc<AtomicU32>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    media_generation: u64,
    current_media_generation: Arc<AtomicU64>,
    events: EventEmitter,
) {
    let _ = thread::Builder::new()
        .name("miplay-audio".to_owned())
        .spawn(move || {
            let key_fingerprint = keys.as_ref().map(StreamKeys::fingerprint);
            if let Err(error) = run_media_reader(
                stream,
                keys,
                output_device,
                paused,
                volume_percent,
                presentation_clock,
                latency_telemetry,
                media_generation,
                Arc::clone(&current_media_generation),
                Arc::clone(&events),
            ) {
                if current_media_generation.load(Ordering::Acquire) != media_generation {
                    return;
                }
                if emit_normal_channel_close(&events, &error, media_generation) {
                    return;
                }
                events(json!({
                    "event": "error",
                    "code": "miplay_audio_stream_failed",
                    "media_generation": media_generation,
                    "key_fingerprint": key_fingerprint,
                    "message": format!("小米妙播音频流失败：{error:#}"),
                }));
            }
        });
}

fn run_media_reader(
    mut stream: TcpStream,
    keys: Option<StreamKeys>,
    output_device: Option<String>,
    paused: Arc<AtomicBool>,
    volume_percent: Arc<AtomicU32>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    media_generation: u64,
    current_media_generation: Arc<AtomicU64>,
    events: EventEmitter,
) -> Result<()> {
    stream
        .set_read_timeout(Some(StdDuration::from_millis(500)))
        .ok();
    let mut player = AacPlayer::new(
        output_device.as_deref(),
        paused,
        volume_percent,
        presentation_clock,
        Arc::clone(&latency_telemetry),
        Arc::clone(&events),
    )?;
    let encrypted_media_expected = keys.is_some();
    let mut demuxer = TsDemuxer::new(keys);
    let mut first_packet = true;
    // Reuse the largest allocation seen on this connection. MiPlay sends one
    // RTP packet every few milliseconds, so allocating a fresh Vec for every
    // packet creates avoidable allocator traffic on the audio reader thread.
    let mut rtp = Vec::with_capacity(2_048);
    loop {
        let mut header = [0_u8; 4];
        if !read_exact_for_generation(
            &mut stream,
            &mut header,
            media_generation,
            &current_media_generation,
        )? {
            return Ok(());
        }
        if header[0] != b'$' {
            bail!("unexpected MiPlay data marker 0x{:02x}", header[0]);
        }
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        if !(12..=65_535).contains(&length) {
            bail!("invalid MiPlay RTP frame length {length}");
        }
        rtp.resize(length, 0);
        if !read_exact_for_generation(
            &mut stream,
            &mut rtp,
            media_generation,
            &current_media_generation,
        )? {
            return Ok(());
        }
        let arrival = Instant::now();
        let rtp_packet = parse_rtp_packet(&rtp).context("parse MiPlay RTP packet")?;
        latency_telemetry.observe_rtp(
            rtp_packet.sequence_number,
            rtp_packet.timestamp,
            rtp.len(),
            arrival,
        );
        for packet in rtp_packet.payload.chunks_exact(188) {
            for audio_pes in demuxer.push_ts_at(packet, Some(rtp_packet.timestamp_micros))? {
                let aac = &audio_pes.payload;
                if first_packet {
                    first_packet = false;
                    let source_sample_rate = adts_sample_rate(aac).unwrap_or(0);
                    let bitrate = adts_bitrate(aac).unwrap_or(0);
                    events(json!({
                        "event": "audio_format",
                        "codec": "AAC-LC",
                        "sample_rate": source_sample_rate,
                        "required_sample_rate": MIPLAY_SAMPLE_RATE_HZ,
                        "channels": 2,
                        "bits_per_sample": 16,
                        "bitrate": bitrate,
                        "encrypted": encrypted_media_expected,
                        "transport": "RTP/MP2T/TCP",
                    }));
                }
                player.decode_adts(aac, audio_pes.pts_micros)?;
            }
        }
    }
}

fn read_exact_for_generation(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    media_generation: u64,
    current_media_generation: &AtomicU64,
) -> Result<bool> {
    let mut offset = 0;
    while offset < buffer.len() {
        if current_media_generation.load(Ordering::Acquire) != media_generation {
            return Ok(false);
        }
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(RemoteChannelClosed::new("audio", offset, buffer.len()).into());
            }
            Ok(read) => offset += read,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error).context("read MiPlay media stream"),
        }
    }
    Ok(true)
}

struct RtpPacketView<'a> {
    payload: &'a [u8],
    sequence_number: u16,
    timestamp: u32,
    timestamp_micros: i64,
}

fn parse_rtp_packet(packet: &[u8]) -> Option<RtpPacketView<'_>> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let sequence_number = u16::from_be_bytes(packet[2..4].try_into().ok()?);
    let rtp_timestamp = u32::from_be_bytes(packet[4..8].try_into().ok()?);
    let mut offset = 12 + usize::from(packet[0] & 0x0f) * 4;
    if packet[0] & 0x10 != 0 {
        if packet.len() < offset + 4 {
            return None;
        }
        let words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        offset += 4 + words * 4;
    }
    let mut end = packet.len();
    if packet[0] & 0x20 != 0 {
        end = end.checked_sub(*packet.last()? as usize)?;
    }
    (offset <= end).then_some(RtpPacketView {
        payload: &packet[offset..end],
        sequence_number,
        timestamp: rtp_timestamp,
        // Payload type 33 is RTP/MP2T and follows the mandatory 90 kHz clock.
        // The contained AAC remains 48 kHz, while both RTP and PES timestamps
        // advance by 1,920 ticks for each 1,024-sample AAC frame.
        timestamp_micros: i64::from(rtp_timestamp) * 1_000_000 / i64::from(MIPLAY_RTP_CLOCK_HZ),
    })
}

#[cfg(test)]
fn rtp_payload(packet: &[u8]) -> Option<&[u8]> {
    parse_rtp_packet(packet).map(|packet| packet.payload)
}

#[derive(Debug)]
struct AudioPes {
    payload: Vec<u8>,
    pts_micros: Option<i64>,
}

struct TsDemuxer {
    pes: Vec<u8>,
    pes_rtp_pts_micros: Option<i64>,
    keys: Option<StreamKeys>,
}

impl TsDemuxer {
    fn new(keys: Option<StreamKeys>) -> Self {
        Self {
            pes: Vec::new(),
            pes_rtp_pts_micros: None,
            keys,
        }
    }

    #[cfg(test)]
    fn push_ts(&mut self, packet: &[u8]) -> Result<Vec<AudioPes>> {
        self.push_ts_at(packet, None)
    }

    fn push_ts_at(&mut self, packet: &[u8], rtp_pts_micros: Option<i64>) -> Result<Vec<AudioPes>> {
        if packet.len() != 188 || packet[0] != 0x47 {
            return Ok(Vec::new());
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        if pid != 0x1100 {
            return Ok(Vec::new());
        }
        let adaptation = (packet[3] >> 4) & 0x03;
        if adaptation == 0 || adaptation == 2 {
            return Ok(Vec::new());
        }
        let mut offset = 4_usize;
        if adaptation == 3 {
            offset = 5 + usize::from(packet[4]);
        }
        if offset >= packet.len() {
            return Ok(Vec::new());
        }

        let mut output = Vec::new();
        if packet[1] & 0x40 != 0 {
            if !self.pes.is_empty()
                && let Some(aac) = self.finish_pes()?
            {
                output.push(aac);
            }
            self.pes_rtp_pts_micros = rtp_pts_micros;
        }
        self.pes.extend_from_slice(&packet[offset..]);
        if self.pes.len() >= 6 {
            let declared = u16::from_be_bytes([self.pes[4], self.pes[5]]) as usize;
            if declared > 0
                && self.pes.len() >= declared + 6
                && let Some(aac) = self.finish_pes()?
            {
                output.push(aac);
            }
        }
        Ok(output)
    }

    fn finish_pes(&mut self) -> Result<Option<AudioPes>> {
        let mut pes = std::mem::take(&mut self.pes);
        let rtp_pts_micros = self.pes_rtp_pts_micros.take();
        if pes.len() < 9 || pes[..4] != [0, 0, 1, 0xc0] {
            return Ok(None);
        }
        let declared = u16::from_be_bytes([pes[4], pes[5]]) as usize;
        if declared > 0 && declared + 6 < pes.len() {
            pes.truncate(declared + 6);
        }
        let payload_offset = 9 + usize::from(pes[8]);
        if payload_offset >= pes.len() {
            return Ok(None);
        }
        // Xiaomi's presentation offset is defined against the MPEG-TS/PES
        // timeline. The RTP timestamp uses a separate 48 kHz transport clock
        // and can have an unrelated epoch, so using it while PES PTS is
        // available can schedule valid audio arbitrarily far in the future.
        // Keep RTP time only as a compatibility fallback for malformed PES.
        let pts_micros = pes_pts_micros(&pes).or(rtp_pts_micros);
        let mut elementary = pes[payload_offset..].to_vec();
        if is_adts(&elementary) {
            return Ok(Some(AudioPes {
                payload: elementary,
                pts_micros,
            }));
        }
        let Some(keys) = self.keys.as_mut() else {
            return Ok(Some(AudioPes {
                payload: elementary,
                pts_micros,
            }));
        };
        if let Some(iv) = pes_private_data_iv(&pes, payload_offset) {
            keys.stream_iv = iv;
        }
        let encrypted_len = elementary.len().min(256) / 16 * 16;
        if encrypted_len > 0 {
            let decryptor =
                cbc::Decryptor::<Aes128>::new_from_slices(&keys.stream_key, &keys.stream_iv)
                    .context("initialize media AES-CBC")?;
            decryptor
                .decrypt_padded::<NoPadding>(&mut elementary[..encrypted_len])
                .map_err(|_| anyhow::anyhow!("decrypt media AES-CBC prefix"))?;
        }
        Ok(Some(AudioPes {
            payload: elementary,
            pts_micros,
        }))
    }
}

fn pes_pts_micros(pes: &[u8]) -> Option<i64> {
    if pes.len() < 14 || pes[7] & 0x80 == 0 || pes[8] < 5 {
        return None;
    }
    let encoded = &pes[9..14];
    if encoded[0] & 0x01 == 0 || encoded[2] & 0x01 == 0 || encoded[4] & 0x01 == 0 {
        return None;
    }
    let pts = (u64::from((encoded[0] >> 1) & 0x07) << 30)
        | (u64::from(encoded[1]) << 22)
        | (u64::from(encoded[2] >> 1) << 15)
        | (u64::from(encoded[3]) << 7)
        | u64::from(encoded[4] >> 1);
    Some((pts.saturating_mul(1_000_000) / 90_000).min(i64::MAX as u64) as i64)
}

fn pes_private_data_iv(pes: &[u8], payload_offset: usize) -> Option<[u8; 16]> {
    if pes.len() < 9 || payload_offset > pes.len() {
        return None;
    }
    let flags = pes[7];
    if flags & 0x01 == 0 {
        return None;
    }

    let mut cursor = 9_usize;
    if flags & 0x80 != 0 {
        cursor = cursor.checked_add(5)?;
        if flags & 0xc0 == 0xc0 {
            cursor = cursor.checked_add(5)?;
        }
    }
    if flags & 0x20 != 0 {
        cursor = cursor.checked_add(6)?;
    }
    if flags & 0x10 != 0 {
        cursor = cursor.checked_add(3)?;
    }

    // PES_extension_flag is followed by the extension flags byte. Bit 7
    // denotes PES_private_data_flag, followed immediately by 16 IV bytes.
    if cursor.checked_add(17)? > payload_offset || pes.get(cursor)? & 0x80 == 0 {
        return None;
    }
    pes.get(cursor + 1..cursor + 17)?.try_into().ok()
}

fn is_adts(data: &[u8]) -> bool {
    data.len() >= 7 && has_adts_sync(data)
}

fn has_adts_sync(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xff && data[1] & 0xf6 == 0xf0
}

fn adts_header_len(data: &[u8]) -> Option<usize> {
    is_adts(data).then_some(if data[1] & 1 == 0 { 9 } else { 7 })
}

fn adts_frame_len(data: &[u8]) -> Option<usize> {
    is_adts(data).then_some(
        (usize::from(data[3] & 0x03) << 11)
            | (usize::from(data[4]) << 3)
            | (usize::from(data[5]) >> 5),
    )
}

fn adts_sample_rate(data: &[u8]) -> Option<u32> {
    const SAMPLE_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    is_adts(data).then_some(())?;
    let frequency_index = usize::from((data[2] & 0x3c) >> 2);
    SAMPLE_RATES.get(frequency_index).copied()
}

fn validate_miplay_sample_rate(data: &[u8]) -> Result<u32> {
    let sample_rate = adts_sample_rate(data).context("ADTS sample rate is missing or reserved")?;
    if sample_rate != MIPLAY_SAMPLE_RATE_HZ {
        bail!(
            "unsupported MiPlay AAC sample rate {sample_rate} Hz; expected {MIPLAY_SAMPLE_RATE_HZ} Hz"
        );
    }
    Ok(sample_rate)
}

fn adts_bitrate(data: &[u8]) -> Option<u64> {
    let bytes = adts_frame_len(data)?;
    let sample_rate = adts_sample_rate(data)?;
    Some(bytes as u64 * 8 * u64::from(sample_rate) / 1024)
}

struct PcmQueue {
    samples: VecDeque<f32>,
    capacity: usize,
    read_phase: f64,
    synchronized: bool,
    front_pts_micros: Option<f64>,
}

impl PcmQueue {
    fn new() -> Self {
        let capacity =
            MIPLAY_SOURCE_SAMPLE_RATE * MIPLAY_SOURCE_CHANNELS * MIPLAY_MAX_BUFFER_MILLIS / 1_000;
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            read_phase: 0.0,
            synchronized: false,
            front_pts_micros: None,
        }
    }

    #[cfg(test)]
    fn push(&mut self, input: &[f32]) {
        self.push_timed(input, None);
    }

    fn push_timed(&mut self, input: &[f32], pts_micros: Option<f64>) {
        let input_frames = input.len() / MIPLAY_SOURCE_CHANNELS;
        if input_frames == 0 {
            return;
        }
        if let (Some(front), Some(pts)) = (self.front_pts_micros, pts_micros) {
            let expected = front
                + self.buffered_frames() as f64 * 1_000_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
            if (pts - expected).abs() > MIPLAY_PTS_DISCONTINUITY_MICROS {
                self.clear();
            }
        }
        if self.samples.is_empty() {
            self.front_pts_micros = pts_micros;
        } else if self.front_pts_micros.is_none()
            && let Some(pts) = pts_micros
        {
            self.front_pts_micros = Some(
                pts - self.buffered_frames() as f64 * 1_000_000.0
                    / MIPLAY_SOURCE_SAMPLE_RATE as f64,
            );
        }
        let overflow = self
            .samples
            .len()
            .saturating_add(input.len())
            .saturating_sub(self.capacity);
        if overflow > 0 {
            let overflow = overflow.div_ceil(MIPLAY_SOURCE_CHANNELS) * MIPLAY_SOURCE_CHANNELS;
            let dropped_samples = overflow.min(self.samples.len());
            self.samples.drain(..dropped_samples);
            self.advance_pts(dropped_samples / MIPLAY_SOURCE_CHANNELS);
        }
        self.samples.extend(input.iter().copied());
    }

    fn buffered_frames(&self) -> usize {
        self.samples.len() / MIPLAY_SOURCE_CHANNELS
    }

    fn ready_to_play(&mut self, target_frames: usize) -> bool {
        if !self.synchronized && self.buffered_frames() >= target_frames {
            self.synchronized = true;
        }
        self.synchronized
    }

    #[cfg(not(target_os = "android"))]
    fn playback_rate(&self, target_frames: usize, nominal_rate: f64) -> f64 {
        let error_frames = self.buffered_frames() as f64 - target_frames as f64;
        let deadband_frames =
            MIPLAY_SOURCE_SAMPLE_RATE as f64 * MIPLAY_SYNC_DEADBAND_MILLIS as f64 / 1_000.0;
        if error_frames.abs() <= deadband_frames {
            return nominal_rate;
        }
        let correction = (error_frames
            / (MIPLAY_SOURCE_SAMPLE_RATE as f64 * MIPLAY_DRIFT_CORRECTION_SECONDS))
            .clamp(-MIPLAY_MAX_RATE_CORRECTION, MIPLAY_MAX_RATE_CORRECTION);
        nominal_rate * (1.0 + correction)
    }

    #[cfg(not(target_os = "android"))]
    fn scheduled_playback_rate(&self, schedule_error_micros: f64, nominal_rate: f64) -> f64 {
        if schedule_error_micros.abs() <= MIPLAY_SYNC_DEADBAND_MILLIS as f64 * 1_000.0 {
            return nominal_rate;
        }
        let correction = (-schedule_error_micros / (MIPLAY_DRIFT_CORRECTION_SECONDS * 1_000_000.0))
            .clamp(-MIPLAY_MAX_RATE_CORRECTION, MIPLAY_MAX_RATE_CORRECTION);
        nominal_rate * (1.0 + correction)
    }

    fn front_pts_micros(&self) -> Option<f64> {
        self.front_pts_micros
    }

    fn drop_frames(&mut self, frames: usize) -> usize {
        let dropped_samples = frames.saturating_mul(MIPLAY_SOURCE_CHANNELS).min(
            self.samples
                .len()
                .saturating_sub(MIPLAY_SOURCE_CHANNELS * 2),
        );
        self.samples.drain(..dropped_samples);
        let dropped_frames = dropped_samples / MIPLAY_SOURCE_CHANNELS;
        self.advance_pts(dropped_frames);
        dropped_frames
    }

    fn advance_pts(&mut self, frames: usize) {
        if let Some(pts) = self.front_pts_micros.as_mut() {
            *pts += frames as f64 * 1_000_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
        }
    }

    #[cfg(not(target_os = "android"))]
    fn render_stereo(&mut self, playback_rate: f64) -> (f32, f32) {
        if self.samples.len() < MIPLAY_SOURCE_CHANNELS * 2 {
            self.synchronized = false;
            self.read_phase = 0.0;
            return (0.0, 0.0);
        }

        let left = self.samples[0];
        let right = self.samples[1];
        let next_left = self.samples[2];
        let next_right = self.samples[3];
        let phase = self.read_phase as f32;
        let rendered = (
            left + (next_left - left) * phase,
            right + (next_right - right) * phase,
        );

        self.read_phase += playback_rate.max(0.0);
        let consumed_frames = self.read_phase.floor() as usize;
        if consumed_frames > 0 {
            let consumed_samples = consumed_frames
                .saturating_mul(MIPLAY_SOURCE_CHANNELS)
                .min(self.samples.len().saturating_sub(MIPLAY_SOURCE_CHANNELS));
            self.samples.drain(..consumed_samples);
            let consumed_frames = consumed_samples / MIPLAY_SOURCE_CHANNELS;
            self.advance_pts(consumed_frames);
            self.read_phase -= consumed_frames as f64;
        }
        rendered
    }

    #[cfg(any(target_os = "android", test))]
    fn render_stereo_block(&mut self, output: &mut [(f32, f32)], playback_rate: f64) -> f32 {
        let available_frames = self.buffered_frames();
        if available_frames < 2 {
            self.synchronized = false;
            self.read_phase = 0.0;
            output.fill((0.0, 0.0));
            return 0.0;
        }

        let playback_rate = playback_rate.max(0.0);
        let mut source_position = self.read_phase;
        let mut callback_peak = 0.0_f32;
        let mut exhausted = false;
        for frame in output.iter_mut() {
            let source_frame = source_position.floor() as usize;
            if source_frame + 1 >= available_frames {
                *frame = (0.0, 0.0);
                exhausted = true;
                continue;
            }

            let sample_index = source_frame * MIPLAY_SOURCE_CHANNELS;
            let phase = (source_position - source_frame as f64) as f32;
            let left = self.samples[sample_index];
            let right = self.samples[sample_index + 1];
            let next_left = self.samples[sample_index + MIPLAY_SOURCE_CHANNELS];
            let next_right = self.samples[sample_index + MIPLAY_SOURCE_CHANNELS + 1];
            frame.0 = left + (next_left - left) * phase;
            frame.1 = right + (next_right - right) * phase;
            callback_peak = callback_peak.max(frame.0.abs()).max(frame.1.abs());
            source_position += playback_rate;
        }

        let consumed_frames = (source_position.floor() as usize).min(available_frames - 1);
        if consumed_frames > 0 {
            self.samples
                .drain(..consumed_frames * MIPLAY_SOURCE_CHANNELS);
            self.advance_pts(consumed_frames);
        }
        if exhausted {
            self.synchronized = false;
            self.read_phase = 0.0;
        } else {
            self.read_phase = source_position - consumed_frames as f64;
        }
        callback_peak
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.read_phase = 0.0;
        self.synchronized = false;
        self.front_pts_micros = None;
    }
}

fn synchronized_buffer_frames(output_latency_millis: f64) -> usize {
    let queue_latency_millis = (MIPLAY_GROUP_PLAY_DELAY_MILLIS as f64 - output_latency_millis)
        .max(MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS as f64);
    (MIPLAY_SOURCE_SAMPLE_RATE as f64 * queue_latency_millis / 1_000.0).round() as usize
}

fn normalized_output_latency_millis(
    observed_latency_millis: Option<f64>,
    output_sample_rate: u32,
    callback_frames: usize,
) -> f64 {
    observed_latency_millis
        .filter(|latency| latency.is_finite() && *latency >= 0.0)
        .unwrap_or_else(|| callback_frames as f64 * 1_000.0 / f64::from(output_sample_rate.max(1)))
        .min(MIPLAY_MAX_OUTPUT_LATENCY_MILLIS)
}

#[cfg(not(target_os = "android"))]
fn output_latency_millis(
    info: &cpal::OutputCallbackInfo,
    output_sample_rate: u32,
    callback_frames: usize,
) -> f64 {
    let timestamp = info.timestamp();
    let observed = timestamp
        .playback
        .duration_since(&timestamp.callback)
        .map(|duration| duration.as_secs_f64() * 1_000.0);
    normalized_output_latency_millis(observed, output_sample_rate, callback_frames)
}

fn presentation_schedule_error_micros(
    media_pts_micros: f64,
    presentation_offset_micros: i64,
    synchronized_server_now_micros: i64,
    output_latency_millis: f64,
) -> f64 {
    media_pts_micros + presentation_offset_micros as f64
        - (synchronized_server_now_micros as f64 + output_latency_millis * 1_000.0)
}

fn usable_presentation_schedule_error_micros(
    media_pts_micros: f64,
    presentation_offset_micros: i64,
    synchronized_server_now_micros: i64,
    output_latency_millis: f64,
) -> Option<f64> {
    let schedule_error = presentation_schedule_error_micros(
        media_pts_micros,
        presentation_offset_micros,
        synchronized_server_now_micros,
        output_latency_millis,
    );
    let maximum_buffer_micros = MIPLAY_MAX_BUFFER_MILLIS as f64 * 1_000.0;
    (schedule_error.is_finite() && schedule_error.abs() <= maximum_buffer_micros)
        .then_some(schedule_error)
}

fn should_hard_drop_late_frames(
    timed_playback_started: bool,
    schedule_error_micros: f64,
    allow_running_drop: bool,
) -> bool {
    if timed_playback_started && !allow_running_drop {
        return false;
    }
    let threshold = if timed_playback_started {
        MIPLAY_RUNNING_LATE_DROP_MICROS
    } else {
        MIPLAY_INITIAL_LATE_DROP_MICROS
    };
    schedule_error_micros < -threshold
}

const MIPLAY_SAMPLE_RATE_HZ: u32 = 48_000;
const MIPLAY_RTP_CLOCK_HZ: u32 = 90_000;
const MIPLAY_SOURCE_SAMPLE_RATE: usize = MIPLAY_SAMPLE_RATE_HZ as usize;
const MIPLAY_SOURCE_CHANNELS: usize = 2;
const MIPLAY_GROUP_PLAY_DELAY_MILLIS: usize = 800;
const MIPLAY_MINIMUM_QUEUE_DELAY_MILLIS: usize = 650;
const MIPLAY_MAX_BUFFER_MILLIS: usize = 1_500;
const MIPLAY_MAX_OUTPUT_LATENCY_MILLIS: f64 = 250.0;
#[cfg(not(target_os = "android"))]
const MIPLAY_SYNC_DEADBAND_MILLIS: usize = 2;
#[cfg(not(target_os = "android"))]
const MIPLAY_DRIFT_CORRECTION_SECONDS: f64 = 12.0;
#[cfg(not(target_os = "android"))]
const MIPLAY_MAX_RATE_CORRECTION: f64 = 0.0025;
#[cfg(target_os = "android")]
const MIPLAY_ANDROID_PLAYBACK_RATE: f64 = 1.0;
const MIPLAY_PTS_DISCONTINUITY_MICROS: f64 = 100_000.0;
const MIPLAY_INITIAL_LATE_DROP_MICROS: f64 = 2_000.0;
const MIPLAY_RUNNING_LATE_DROP_MICROS: f64 = 25_000.0;
const MIPLAY_TIMER_PACKET_BYTES: usize = 40;
const MIPLAY_CLOCK_SYNC_SAMPLES: usize = 10;
const MIPLAY_CLOCK_FILTER_SAMPLES: usize = 10;
const MIPLAY_CLOCK_FIT_HISTORY: usize = 32;
const MIPLAY_CLOCK_MIN_FIT_ROUNDS: usize = 20;
const MIPLAY_CLOCK_RESYNC_POLLS: usize = 50;
const MIPLAY_CLOCK_FORCE_POLL_MILLIS: u64 = 100;
const MIPLAY_CLOCK_SAMPLE_INTERVAL_MILLIS: u64 = 10;
const MIPLAY_CLOCK_MAX_RTT_MICROS: i64 = 250_000;
const MIPLAY_CLOCK_MIN_FIT_SPAN_MICROS: i64 = 5_000_000;
const MIPLAY_CLOCK_MAX_FREQUENCY_ERROR: f64 = 0.0005;
const MIPLAY_CLOCK_PHASE_SMOOTHING: f64 = 0.25;
const MIPLAY_CLOCK_RATE_SMOOTHING: f64 = 0.25;
const MIPLAY_CLOCK_MAX_PHASE_STEP_MICROS: i64 = 5_000;
const MIPLAY_AAC_FRAME_MICROS: f64 = 1024.0 * 1_000_000.0 / MIPLAY_SAMPLE_RATE_HZ as f64;
const MIPLAY_LATENCY_REPORT_INTERVAL: StdDuration = StdDuration::from_millis(250);
const MIPLAY_RTSP_POLL_INTERVAL: StdDuration = StdDuration::from_millis(50);
const MIPLAY_BITRATE_WINDOW: StdDuration = StdDuration::from_secs(2);
const MIPLAY_MINIMUM_BITRATE_WINDOW: StdDuration = StdDuration::from_millis(250);
const MIPLAY_RTP_REGRESSION_SAMPLES: usize = 128;
const MIPLAY_OUTPUT_LATENCY_REFRESH_CALLBACKS: u32 = 64;
const MIPLAY_OUTPUT_LATENCY_SMOOTHING: f64 = 0.2;

struct AacPlayer {
    decoder: AacDecoder,
    queue: Arc<Mutex<PcmQueue>>,
    _stream: NativeAudioStream,
    adts_framer: AdtsFramer,
    timestamp: i64,
    next_media_pts_micros: Option<f64>,
    decoded_samples: Vec<f32>,
    decoded_frames: u64,
    non_silent_diagnostic_emitted: bool,
    pes_layout_diagnostic_emitted: bool,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
}

#[cfg(not(target_os = "android"))]
type NativeAudioStream = cpal::Stream;

#[cfg(target_os = "android")]
type NativeAudioStream = AndroidMiPlayAudioOutput;

#[cfg(target_os = "android")]
type AndroidMiPlayOboeStream = oboe::AudioStreamAsync<oboe::Output, AndroidMiPlayAudioCallback>;

#[cfg(target_os = "android")]
enum AndroidMiPlayAudioCommand {
    Restart(OboeError),
    Shutdown,
}

#[cfg(target_os = "android")]
struct AndroidMiPlayAudioOutput {
    commands: mpsc::Sender<AndroidMiPlayAudioCommand>,
}

#[cfg(target_os = "android")]
impl Drop for AndroidMiPlayAudioOutput {
    fn drop(&mut self) {
        let _ = self.commands.send(AndroidMiPlayAudioCommand::Shutdown);
    }
}

impl AacPlayer {
    fn new(
        requested_device: Option<&str>,
        paused: Arc<AtomicBool>,
        volume_percent: Arc<AtomicU32>,
        presentation_clock: Arc<MiPlayPresentationClock>,
        latency_telemetry: Arc<MiPlayLatencyTelemetry>,
        events: EventEmitter,
    ) -> Result<Self> {
        let mut parameters = AudioCodecParameters::new();
        parameters
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(MIPLAY_SAMPLE_RATE_HZ)
            .with_channels(Channels::from(Position::FRONT_LEFT | Position::FRONT_RIGHT));
        let decoder = AacDecoder::try_new(&parameters, &AudioDecoderOptions::default())
            .context("initialize AAC-LC decoder")?;

        let (queue, stream) = open_native_audio_output(
            requested_device,
            paused,
            volume_percent,
            presentation_clock,
            Arc::clone(&latency_telemetry),
            Arc::clone(&events),
        )?;

        Ok(Self {
            decoder,
            queue,
            _stream: stream,
            adts_framer: AdtsFramer::default(),
            timestamp: 0,
            next_media_pts_micros: None,
            decoded_samples: Vec::with_capacity(2_048),
            decoded_frames: 0,
            non_silent_diagnostic_emitted: false,
            pes_layout_diagnostic_emitted: false,
            latency_telemetry,
            events,
        })
    }

    fn decode_adts(&mut self, adts: &[u8], pes_pts_micros: Option<i64>) -> Result<()> {
        if let Some(pts) = pes_pts_micros {
            self.next_media_pts_micros = Some(pts as f64);
        }
        let frames = self.adts_framer.push(adts);
        let frames_in_pes = frames.len() as u32;
        for frame in frames {
            let frame_pts = self.next_media_pts_micros;
            self.decode_adts_frame(&frame, frame_pts)?;
            if let Some(pts) = self.next_media_pts_micros.as_mut() {
                *pts += MIPLAY_AAC_FRAME_MICROS;
            }
        }
        if !self.pes_layout_diagnostic_emitted {
            self.pes_layout_diagnostic_emitted = true;
            (self.events)(json!({
                "event": "aac_pes_layout",
                "protocol": "xiaomi_miplay",
                "pes_payload_bytes": adts.len(),
                "frames_in_pes": frames_in_pes,
                "buffered_adts_bytes": self.adts_framer.buffered_len(),
            }));
        }
        Ok(())
    }

    fn decode_adts_frame(&mut self, adts: &[u8], media_pts_micros: Option<f64>) -> Result<()> {
        validate_miplay_sample_rate(adts)?;
        let header_len = adts_header_len(adts).context("invalid ADTS header")?;
        let frame_len = adts_frame_len(adts).context("invalid ADTS length")?;
        if frame_len > adts.len() || frame_len <= header_len {
            bail!(
                "truncated ADTS frame: declared {frame_len}, received {}",
                adts.len()
            );
        }
        let packet = Packet::new(
            0,
            Timestamp::new(self.timestamp),
            Duration::new(1024),
            adts[header_len..frame_len].to_vec(),
        );
        self.timestamp = self.timestamp.saturating_add(1024);
        let decoded = self
            .decoder
            .decode(&packet)
            .context("decode AAC-LC frame")?;
        self.decoded_samples
            .resize(decoded.samples_interleaved(), 0.0);
        decoded.copy_to_slice_interleaved(&mut self.decoded_samples);
        self.decoded_frames = self.decoded_frames.saturating_add(1);
        let checkpoint = matches!(self.decoded_frames, 1 | 8 | 32 | 128);
        if checkpoint || !self.non_silent_diagnostic_emitted {
            let peak = self
                .decoded_samples
                .iter()
                .fold(0.0_f32, |current, value| current.max(value.abs()));
            let non_silent = peak > 0.000_01;
            if checkpoint || non_silent {
                let rms = if self.decoded_samples.is_empty() {
                    0.0
                } else {
                    (self
                        .decoded_samples
                        .iter()
                        .map(|value| value * value)
                        .sum::<f32>()
                        / self.decoded_samples.len() as f32)
                        .sqrt()
                };
                self.non_silent_diagnostic_emitted |= non_silent;
                let payload = &adts[header_len..frame_len];
                (self.events)(json!({
                    "event": "audio_pcm_decoded",
                    "protocol": "xiaomi_miplay",
                    "decoded_frames": self.decoded_frames,
                    "adts_frame_bytes": frame_len,
                    "adts_header_hex": hex::encode(&adts[..header_len]),
                    "aac_payload_prefix_hex": hex::encode(&payload[..payload.len().min(32)]),
                    "aac_first_element_id": payload.first().map(|byte| byte >> 5),
                    "samples": self.decoded_samples.len(),
                    "peak": peak,
                    "rms": rms,
                    "non_silent": non_silent,
                }));
            }
        }
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_timed(&self.decoded_samples, media_pts_micros);
            self.latency_telemetry
                .update_buffered_frames(queue.buffered_frames());
        }
        Ok(())
    }
}

const MAX_BUFFERED_ADTS_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct AdtsFramer {
    pending: Vec<u8>,
}

impl AdtsFramer {
    fn push(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(input);
        let mut frames = Vec::new();
        let mut consumed = 0;

        loop {
            let remaining = &self.pending[consumed..];
            if remaining.len() < 2 {
                break;
            }
            if !has_adts_sync(remaining) {
                let next_sync = remaining
                    .windows(2)
                    .skip(1)
                    .position(|window| window[0] == 0xff && window[1] & 0xf6 == 0xf0)
                    .map(|offset| offset + 1);
                match next_sync {
                    Some(offset) => {
                        consumed += offset;
                        continue;
                    }
                    None => {
                        let keep_last_sync_byte = self.pending.last() == Some(&0xff);
                        let last = self.pending.last().copied();
                        self.pending.clear();
                        if keep_last_sync_byte {
                            self.pending.push(last.unwrap_or(0xff));
                        }
                        // The whole backing buffer, including any frames
                        // already copied above, has just been discarded.
                        consumed = 0;
                        break;
                    }
                }
            }
            if remaining.len() < 7 {
                break;
            }

            let Some(frame_len) = adts_frame_len(remaining) else {
                break;
            };
            let header_len = adts_header_len(remaining).unwrap_or(7);
            if frame_len <= header_len {
                consumed += 1;
                continue;
            }
            if remaining.len() < frame_len {
                break;
            }
            frames.push(remaining[..frame_len].to_vec());
            consumed += frame_len;
        }

        // Removing every frame separately shifts the tail repeatedly when a
        // PES contains several AAC frames. Drain once after the linear scan.
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        if self.pending.len() == 1 && self.pending[0] != 0xff {
            self.pending.clear();
        }

        if self.pending.len() > MAX_BUFFERED_ADTS_BYTES {
            let keep_last_sync_byte = self.pending.last() == Some(&0xff);
            self.pending.clear();
            if keep_last_sync_byte {
                self.pending.push(0xff);
            }
        }
        frames
    }

    fn buffered_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(not(target_os = "android"))]
fn open_native_audio_output(
    requested_device: Option<&str>,
    paused: Arc<AtomicBool>,
    volume_percent: Arc<AtomicU32>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
) -> Result<(Arc<Mutex<PcmQueue>>, NativeAudioStream)> {
    let host = cpal::default_host();
    let requested_match = match requested_device {
        Some(requested) => host
            .output_devices()
            .context("enumerate native audio output devices")?
            .find(|candidate| {
                candidate
                    .id()
                    .map(|id| id.to_string().eq_ignore_ascii_case(requested))
                    .unwrap_or(false)
                    || candidate
                        .description()
                        .map(|description| description.name().eq_ignore_ascii_case(requested))
                        .unwrap_or(false)
            }),
        None => None,
    };
    let device = if let Some(device) = requested_match {
        device
    } else {
        if let Some(requested) = requested_device {
            events(json!({
                "event": "warning",
                "code": "miplay_audio_output_fallback",
                "protocol": "xiaomi_miplay",
                "requested_device": requested,
                "message": "Requested audio output is unavailable; using the system default output.",
            }));
        }
        host.default_output_device()
            .context("no native audio output device")?
    };
    let default_supported = device
        .default_output_config()
        .context("read native output format")?;
    let default_sample_format = default_supported.sample_format();
    let default_channels = default_supported.channels();
    let supported = device
        .supported_output_configs()
        .context("enumerate native output formats")?
        .filter(|range| {
            range.min_sample_rate() <= MIPLAY_SAMPLE_RATE_HZ
                && MIPLAY_SAMPLE_RATE_HZ <= range.max_sample_rate()
                && matches!(
                    range.sample_format(),
                    cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
                )
        })
        .min_by_key(|range| {
            (
                range.sample_format() != default_sample_format,
                range.channels() != default_channels,
                range.channels().abs_diff(MIPLAY_SOURCE_CHANNELS as u16),
            )
        })
        .map(|range| range.with_sample_rate(MIPLAY_SAMPLE_RATE_HZ))
        .with_context(|| {
            format!(
                "native output does not support the required MiPlay sample rate {MIPLAY_SAMPLE_RATE_HZ} Hz"
            )
        })?;
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: MIPLAY_SAMPLE_RATE_HZ,
        buffer_size: cpal::BufferSize::Default,
    };
    let device_name = device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| "System default output".to_owned());
    let device_id = device
        .id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| device_name.clone());
    events(json!({
        "event": "audio_output_ready",
        "protocol": "xiaomi_miplay",
        "requested_device": requested_device,
        "device_name": device_name,
        "device_id": device_id,
        "sample_rate": config.sample_rate,
        "channels": config.channels,
        "sample_format": supported.sample_format().to_string(),
    }));
    let queue = Arc::new(Mutex::new(PcmQueue::new()));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_output::<f32>(
            &device,
            &config,
            Arc::clone(&queue),
            paused,
            volume_percent,
            Arc::clone(&presentation_clock),
            Arc::clone(&latency_telemetry),
            Arc::clone(&events),
        ),
        cpal::SampleFormat::I16 => build_output::<i16>(
            &device,
            &config,
            Arc::clone(&queue),
            paused,
            volume_percent,
            Arc::clone(&presentation_clock),
            Arc::clone(&latency_telemetry),
            Arc::clone(&events),
        ),
        cpal::SampleFormat::U16 => build_output::<u16>(
            &device,
            &config,
            Arc::clone(&queue),
            paused,
            volume_percent,
            Arc::clone(&presentation_clock),
            Arc::clone(&latency_telemetry),
            Arc::clone(&events),
        ),
        format => bail!("unsupported native sample format {format}"),
    }
    .context("open native audio output")?;
    stream.play().context("start native audio output")?;
    Ok((queue, stream))
}

#[cfg(target_os = "android")]
struct AndroidMiPlayAudioCallback {
    queue: Arc<Mutex<PcmQueue>>,
    paused: Arc<AtomicBool>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
    commands: mpsc::Sender<AndroidMiPlayAudioCommand>,
    output_sample_rate: Arc<AtomicU32>,
    callback_started: bool,
    signal_started: bool,
    synchronization_started: bool,
    endpoint_latency_millis: f64,
    endpoint_latency_initialized: bool,
    latency_refresh_countdown: u32,
    timed_playback_started: bool,
}

#[cfg(target_os = "android")]
impl AudioOutputCallback for AndroidMiPlayAudioCallback {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        stream: &mut dyn AudioOutputStreamSafe,
        output: &mut [(f32, f32)],
    ) -> DataCallbackResult {
        let callback_frames = output.len();
        let output_sample_rate = self.output_sample_rate.load(Ordering::Acquire).max(1);
        let route_playback_rate = f64::from(MIPLAY_SAMPLE_RATE_HZ) / f64::from(output_sample_rate);
        if !self.endpoint_latency_initialized || self.latency_refresh_countdown == 0 {
            let observed_latency = stream.calculate_latency_millis().ok();
            let measured_latency = normalized_output_latency_millis(
                observed_latency,
                output_sample_rate,
                callback_frames,
            );
            self.endpoint_latency_millis = if self.endpoint_latency_initialized {
                self.endpoint_latency_millis * (1.0 - MIPLAY_OUTPUT_LATENCY_SMOOTHING)
                    + measured_latency * MIPLAY_OUTPUT_LATENCY_SMOOTHING
            } else {
                measured_latency
            };
            self.endpoint_latency_initialized = true;
            self.latency_refresh_countdown = MIPLAY_OUTPUT_LATENCY_REFRESH_CALLBACKS;
        } else {
            self.latency_refresh_countdown -= 1;
        }
        self.latency_telemetry
            .update_output_latency(self.endpoint_latency_millis);
        if !self.callback_started {
            self.callback_started = true;
            (self.events)(json!({
                "event": "audio_output_callback_started",
                "protocol": "xiaomi_miplay",
                "frames": callback_frames,
                "channels": 2,
            }));
        }
        if self.paused.load(Ordering::Acquire) {
            self.timed_playback_started = false;
            if let Ok(mut queue) = self.queue.lock() {
                queue.clear();
            }
            self.latency_telemetry.update_buffered_frames(0);
            output.fill((0.0, 0.0));
            return DataCallbackResult::Continue;
        }
        // The decoder holds this lock only while appending one already-decoded
        // AAC frame. Skipping a callback on contention inserts a 4 ms block of
        // silence roughly at the AAC cadence, which is much more audible than
        // waiting for the short append to finish.
        let Ok(mut queue) = self.queue.lock() else {
            output.fill((0.0, 0.0));
            return DataCallbackResult::Continue;
        };
        self.latency_telemetry
            .update_buffered_frames(queue.buffered_frames());
        let synchronized_frames = synchronized_buffer_frames(self.endpoint_latency_millis);
        let mut leading_silence_frames = 0_usize;
        let mut synchronization_mode = "buffer_window";
        let playback_rate = if self.presentation_clock.synchronization_ready()
            && let Some(front_pts) = queue.front_pts_micros()
            && let Some(server_now) = self.presentation_clock.synchronized_server_now_micros()
            && let Some(mut schedule_error) = usable_presentation_schedule_error_micros(
                front_pts,
                self.presentation_clock
                    .presentation_offset_micros
                    .load(Ordering::Acquire),
                server_now,
                self.endpoint_latency_millis,
            ) {
            synchronization_mode = "sender_clock";
            // Align an already-late stream before the first audible frame, but
            // never splice PCM at an arbitrary waveform position once playback
            // is running. Stable playback catches clock drift through the
            // continuous rate controller below; hard drops here produce clicks
            // even when Android's output track itself has no underruns.
            if should_hard_drop_late_frames(self.timed_playback_started, schedule_error, false) {
                let late_frames = ((-schedule_error) * MIPLAY_SOURCE_SAMPLE_RATE as f64
                    / 1_000_000.0)
                    .floor() as usize;
                let dropped = queue.drop_frames(late_frames);
                schedule_error += dropped as f64 * 1_000_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
            }
            if schedule_error > 0.0 {
                leading_silence_frames =
                    (schedule_error * f64::from(output_sample_rate) / 1_000_000.0).ceil() as usize;
                if leading_silence_frames >= callback_frames {
                    output.fill((0.0, 0.0));
                    return DataCallbackResult::Continue;
                }
            }
            self.timed_playback_started = true;
            // Android already exposes an exact 48 kHz output stream. Applying
            // sender-clock correction by linearly resampling every callback
            // changes the effective sample rate and creates a faint metallic
            // edge. Use the synchronized clock only to select the first audible
            // frame, then keep a strict sample-for-sample 48 kHz path.
            MIPLAY_ANDROID_PLAYBACK_RATE * route_playback_rate
        } else {
            self.timed_playback_started = false;
            if !queue.ready_to_play(synchronized_frames) {
                output.fill((0.0, 0.0));
                return DataCallbackResult::Continue;
            }
            // The 800 ms queue absorbs normal WLAN jitter without continuously
            // pitching the decoded PCM up and down.
            MIPLAY_ANDROID_PLAYBACK_RATE * route_playback_rate
        };
        if !self.synchronization_started {
            self.synchronization_started = true;
            let queue_delay_millis =
                synchronized_frames as f64 * 1_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
            (self.events)(json!({
                "event": "audio_synchronization_started",
                "protocol": "xiaomi_miplay",
                "group_target_delay_ms": MIPLAY_GROUP_PLAY_DELAY_MILLIS,
                "queue_delay_ms": queue_delay_millis,
                "output_latency_ms": self.endpoint_latency_millis,
                "output_callback_frames": callback_frames,
                "output_sample_rate": output_sample_rate,
                "synchronization_mode": synchronization_mode,
            }));
        }
        output[..leading_silence_frames].fill((0.0, 0.0));
        let callback_peak =
            queue.render_stereo_block(&mut output[leading_silence_frames..], playback_rate);
        self.latency_telemetry
            .update_buffered_frames(queue.buffered_frames());
        drop(queue);
        if callback_peak > 0.000_01 && !self.signal_started {
            self.signal_started = true;
            (self.events)(json!({
                "event": "audio_output_signal_started",
                "protocol": "xiaomi_miplay",
                "peak": callback_peak,
            }));
        }
        DataCallbackResult::Continue
    }

    fn on_error_after_close(&mut self, _stream: &mut dyn AudioOutputStreamSafe, error: OboeError) {
        let _ = self
            .commands
            .send(AndroidMiPlayAudioCommand::Restart(error));
    }
}

#[cfg(target_os = "android")]
fn open_native_audio_output(
    requested_device: Option<&str>,
    paused: Arc<AtomicBool>,
    _volume_percent: Arc<AtomicU32>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
) -> Result<(Arc<Mutex<PcmQueue>>, NativeAudioStream)> {
    let queue = Arc::new(Mutex::new(PcmQueue::new()));
    let output_sample_rate = Arc::new(AtomicU32::new(MIPLAY_SAMPLE_RATE_HZ));
    let (commands, command_receiver) = mpsc::channel();
    let initial_stream = open_android_miplay_stream(
        Arc::clone(&queue),
        Arc::clone(&paused),
        Arc::clone(&presentation_clock),
        Arc::clone(&latency_telemetry),
        Arc::clone(&events),
        commands.clone(),
        Arc::clone(&output_sample_rate),
    )?;
    let route_queue = Arc::clone(&queue);
    let route_paused = Arc::clone(&paused);
    let route_clock = Arc::clone(&presentation_clock);
    let route_telemetry = Arc::clone(&latency_telemetry);
    let route_events = Arc::clone(&events);
    let route_commands = commands.clone();
    let route_sample_rate = Arc::clone(&output_sample_rate);
    thread::Builder::new()
        .name("miplay-audio-route".to_owned())
        .spawn(move || {
            manage_android_miplay_output(
                initial_stream,
                command_receiver,
                route_commands,
                route_queue,
                route_paused,
                route_clock,
                route_telemetry,
                route_events,
                route_sample_rate,
            );
        })
        .context("start Android MiPlay audio route monitor")?;
    let stream = AndroidMiPlayAudioOutput { commands };
    events(json!({
        "event": "audio_output_ready",
        "protocol": "xiaomi_miplay",
        "requested_device": requested_device,
        "device_name": "Android system audio output",
        "device_id": "android-default",
        "sample_rate": output_sample_rate.load(Ordering::Acquire),
        "channels": 2,
        "sample_format": "f32",
    }));
    Ok((queue, stream))
}

#[cfg(target_os = "android")]
fn open_android_miplay_stream(
    queue: Arc<Mutex<PcmQueue>>,
    paused: Arc<AtomicBool>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
    commands: mpsc::Sender<AndroidMiPlayAudioCommand>,
    output_sample_rate: Arc<AtomicU32>,
) -> Result<AndroidMiPlayOboeStream> {
    let callback = AndroidMiPlayAudioCallback {
        queue,
        paused,
        presentation_clock,
        latency_telemetry,
        events,
        commands,
        output_sample_rate: Arc::clone(&output_sample_rate),
        callback_started: false,
        signal_started: false,
        synchronization_started: false,
        endpoint_latency_millis: 0.0,
        endpoint_latency_initialized: false,
        latency_refresh_countdown: 0,
        timed_playback_started: false,
    };
    let mut stream = AudioStreamBuilder::default()
        .set_usage(Usage::Media)
        .set_content_type(ContentType::Music)
        .set_performance_mode(PerformanceMode::LowLatency)
        .set_sharing_mode(SharingMode::Shared)
        .set_sample_rate(MIPLAY_SAMPLE_RATE_HZ as i32)
        .set_sample_rate_conversion_quality(SampleRateConversionQuality::Medium)
        .set_format_conversion_allowed(true)
        .set_format::<f32>()
        .set_channel_count::<Stereo>()
        .set_callback(callback)
        .open_stream()
        .context("open Android audio output")?;
    let actual_sample_rate = u32::try_from(stream.get_sample_rate())
        .ok()
        .filter(|rate| *rate > 0)
        .unwrap_or(MIPLAY_SAMPLE_RATE_HZ);
    output_sample_rate.store(actual_sample_rate, Ordering::Release);
    stream.start().context("start Android audio output")?;
    Ok(stream)
}

#[cfg(target_os = "android")]
fn manage_android_miplay_output(
    initial_stream: AndroidMiPlayOboeStream,
    receiver: mpsc::Receiver<AndroidMiPlayAudioCommand>,
    commands: mpsc::Sender<AndroidMiPlayAudioCommand>,
    queue: Arc<Mutex<PcmQueue>>,
    paused: Arc<AtomicBool>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
    output_sample_rate: Arc<AtomicU32>,
) {
    let mut stream = Some(initial_stream);
    while let Ok(command) = receiver.recv() {
        match command {
            AndroidMiPlayAudioCommand::Shutdown => break,
            AndroidMiPlayAudioCommand::Restart(error) => {
                stream.take();
                if let Ok(mut queued) = queue.lock() {
                    queued.clear();
                }
                events(json!({
                    "event": "audio_output_route_restarting",
                    "protocol": "xiaomi_miplay",
                    "reason": error.to_string(),
                }));
                for delay_millis in ANDROID_MIPLAY_AUDIO_RESTART_DELAYS_MILLIS {
                    thread::sleep(StdDuration::from_millis(delay_millis));
                    match open_android_miplay_stream(
                        Arc::clone(&queue),
                        Arc::clone(&paused),
                        Arc::clone(&presentation_clock),
                        Arc::clone(&latency_telemetry),
                        Arc::clone(&events),
                        commands.clone(),
                        Arc::clone(&output_sample_rate),
                    ) {
                        Ok(replacement) => {
                            stream = Some(replacement);
                            events(json!({
                                "event": "audio_output_route_recovered",
                                "protocol": "xiaomi_miplay",
                                "sample_rate": output_sample_rate.load(Ordering::Acquire),
                            }));
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                if stream.is_none() {
                    events(json!({
                        "event": "error",
                        "code": "miplay_audio_output_failed",
                        "message": "Android audio route changed and output recovery failed",
                    }));
                    break;
                }
            }
        }
    }
}

#[cfg(target_os = "android")]
const ANDROID_MIPLAY_AUDIO_RESTART_DELAYS_MILLIS: [u64; 6] = [50, 100, 200, 400, 800, 1_600];

#[cfg(not(target_os = "android"))]
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: Arc<Mutex<PcmQueue>>,
    paused: Arc<AtomicBool>,
    volume_percent: Arc<AtomicU32>,
    presentation_clock: Arc<MiPlayPresentationClock>,
    latency_telemetry: Arc<MiPlayLatencyTelemetry>,
    events: EventEmitter,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let callback_started = Arc::new(AtomicBool::new(false));
    let callback_started_for_audio = Arc::clone(&callback_started);
    let signal_started = Arc::new(AtomicBool::new(false));
    let signal_started_for_audio = Arc::clone(&signal_started);
    let synchronization_started = Arc::new(AtomicBool::new(false));
    let synchronization_started_for_audio = Arc::clone(&synchronization_started);
    let callback_events = Arc::clone(&events);
    let output_sample_rate = config.sample_rate;
    let nominal_playback_rate =
        MIPLAY_SOURCE_SAMPLE_RATE as f64 / f64::from(output_sample_rate.max(1));
    let mut endpoint_latency_millis = 0.0_f64;
    let mut endpoint_latency_initialized = false;
    let mut latency_refresh_countdown = 0_u32;
    let mut timed_playback_started = false;
    device.build_output_stream(
        config,
        move |output: &mut [T], info| {
            let callback_frames = output.len() / channels.max(1);
            if !endpoint_latency_initialized || latency_refresh_countdown == 0 {
                let measured_latency =
                    output_latency_millis(info, output_sample_rate, callback_frames);
                endpoint_latency_millis = if endpoint_latency_initialized {
                    endpoint_latency_millis * (1.0 - MIPLAY_OUTPUT_LATENCY_SMOOTHING)
                        + measured_latency * MIPLAY_OUTPUT_LATENCY_SMOOTHING
                } else {
                    measured_latency
                };
                endpoint_latency_initialized = true;
                latency_refresh_countdown = MIPLAY_OUTPUT_LATENCY_REFRESH_CALLBACKS;
            } else {
                latency_refresh_countdown -= 1;
            }
            latency_telemetry.update_output_latency(endpoint_latency_millis);
            if !callback_started_for_audio.swap(true, Ordering::AcqRel) {
                callback_events(json!({
                    "event": "audio_output_callback_started",
                    "protocol": "xiaomi_miplay",
                    "frames": callback_frames,
                    "channels": channels,
                }));
            }
            if paused.load(Ordering::Acquire) {
                timed_playback_started = false;
                if let Ok(mut queue) = queue.try_lock() {
                    queue.clear();
                }
                latency_telemetry.update_buffered_frames(0);
                for sample in output {
                    *sample = T::from_sample(0.0);
                }
                return;
            }
            let Ok(mut queue) = queue.try_lock() else {
                for sample in output {
                    *sample = T::from_sample(0.0);
                }
                return;
            };
            latency_telemetry.update_buffered_frames(queue.buffered_frames());
            let synchronized_frames = synchronized_buffer_frames(endpoint_latency_millis);
            let mut leading_silence_frames = 0_usize;
            let mut synchronization_mode = "buffer_window";
            let playback_rate = if presentation_clock.synchronization_ready()
                && let Some(front_pts) = queue.front_pts_micros()
                && let Some(server_now) = presentation_clock.synchronized_server_now_micros()
                && let Some(mut schedule_error) = usable_presentation_schedule_error_micros(
                    front_pts,
                    presentation_clock
                        .presentation_offset_micros
                        .load(Ordering::Acquire),
                    server_now,
                    endpoint_latency_millis,
                ) {
                synchronization_mode = "sender_clock";
                if should_hard_drop_late_frames(timed_playback_started, schedule_error, true) {
                    let late_frames = ((-schedule_error) * MIPLAY_SOURCE_SAMPLE_RATE as f64
                        / 1_000_000.0)
                        .floor() as usize;
                    let dropped = queue.drop_frames(late_frames);
                    schedule_error +=
                        dropped as f64 * 1_000_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
                }
                if schedule_error > 0.0 {
                    leading_silence_frames = (schedule_error * f64::from(output_sample_rate)
                        / 1_000_000.0)
                        .ceil() as usize;
                    if leading_silence_frames >= callback_frames {
                        for sample in output {
                            *sample = T::from_sample(0.0);
                        }
                        return;
                    }
                }
                timed_playback_started = true;
                queue.scheduled_playback_rate(schedule_error, nominal_playback_rate)
            } else {
                timed_playback_started = false;
                if !queue.ready_to_play(synchronized_frames) {
                    for sample in output {
                        *sample = T::from_sample(0.0);
                    }
                    return;
                }
                queue.playback_rate(synchronized_frames, nominal_playback_rate)
            };
            if !synchronization_started_for_audio.swap(true, Ordering::AcqRel) {
                let queue_delay_millis =
                    synchronized_frames as f64 * 1_000.0 / MIPLAY_SOURCE_SAMPLE_RATE as f64;
                callback_events(json!({
                    "event": "audio_synchronization_started",
                    "protocol": "xiaomi_miplay",
                    "group_target_delay_ms": MIPLAY_GROUP_PLAY_DELAY_MILLIS,
                    "queue_delay_ms": queue_delay_millis,
                    "output_latency_ms": endpoint_latency_millis,
                    "output_callback_frames": callback_frames,
                    "output_sample_rate": output_sample_rate,
                    "synchronization_mode": synchronization_mode,
                }));
            }
            let mut callback_peak = 0.0_f32;
            let gain = volume_percent.load(Ordering::Acquire).min(100) as f32 / 100.0;
            for (frame_index, frame) in output.chunks_mut(channels.max(1)).enumerate() {
                if frame_index < leading_silence_frames {
                    for sample in frame {
                        *sample = T::from_sample(0.0);
                    }
                    continue;
                }
                let (left, right) = queue.render_stereo(playback_rate);
                let left = left * gain;
                let right = right * gain;
                callback_peak = callback_peak.max(left.abs()).max(right.abs());
                for (index, sample) in frame.iter_mut().enumerate() {
                    let value = match index {
                        0 if channels == 1 => (left + right) * 0.5,
                        0 => left,
                        1 => right,
                        _ => 0.0,
                    };
                    *sample = T::from_sample(value);
                }
            }
            latency_telemetry.update_buffered_frames(queue.buffered_frames());
            if callback_peak > 0.000_01 && !signal_started_for_audio.swap(true, Ordering::AcqRel) {
                callback_events(json!({
                    "event": "audio_output_signal_started",
                    "protocol": "xiaomi_miplay",
                    "peak": callback_peak,
                }));
            }
        },
        move |error| {
            events(json!({
                "event": "error",
                "code": "miplay_audio_output_failed",
                "message": format!("Native audio output error: {error}"),
            }));
        },
        None,
    )
}

#[derive(Debug)]
struct RtspMessage {
    start_line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl RtspMessage {
    fn is_response(&self) -> bool {
        self.start_line.starts_with("RTSP/")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn body_text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }
}

const MAX_RTSP_MESSAGE_BYTES: usize = 1024 * 1024;

fn try_parse_rtsp_message(input: &mut Vec<u8>) -> Result<Option<RtspMessage>> {
    let Some(header_end) = input.windows(4).position(|window| window == b"\r\n\r\n") else {
        if input.len() > MAX_RTSP_MESSAGE_BYTES {
            bail!("MiPlay RTSP header exceeds {MAX_RTSP_MESSAGE_BYTES} bytes");
        }
        return Ok(None);
    };
    let body_start = header_end
        .checked_add(4)
        .context("MiPlay RTSP header length overflow")?;
    if body_start > MAX_RTSP_MESSAGE_BYTES {
        bail!("MiPlay RTSP header exceeds {MAX_RTSP_MESSAGE_BYTES} bytes");
    }

    let header_bytes = &input[..header_end];
    let header_text = std::str::from_utf8(header_bytes).context("RTSP header is not UTF-8")?;
    let mut lines = header_text.split("\r\n");
    let start_line = lines.next().unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let body_len = headers
        .get("content-length")
        .map(|value| {
            value
                .parse::<usize>()
                .context("invalid RTSP Content-Length")
        })
        .transpose()?
        .unwrap_or(0);
    let total = body_start
        .checked_add(body_len)
        .context("MiPlay RTSP message length overflow")?;
    if total > MAX_RTSP_MESSAGE_BYTES {
        bail!("MiPlay RTSP message exceeds {MAX_RTSP_MESSAGE_BYTES} bytes");
    }
    if input.len() < total {
        return Ok(None);
    }

    let body = input[body_start..total].to_vec();
    input.drain(..total);
    Ok(Some(RtspMessage {
        start_line,
        headers,
        body,
    }))
}

fn read_rtsp_message(stream: &mut TcpStream, input: &mut Vec<u8>) -> Result<Option<RtspMessage>> {
    loop {
        if let Some(message) = try_parse_rtsp_message(input)? {
            return Ok(Some(message));
        }

        let mut chunk = [0_u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => return Err(RemoteChannelClosed::new("rtsp", 0, 0).into()),
            Ok(read) => input.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Ok(None);
            }
            Err(error) => return Err(error).context("read RTSP message"),
        }
    }
}

fn send_rtsp_response(stream: &mut TcpStream, cseq: &str, body: &str) -> Result<()> {
    let response = format!(
        "RTSP/1.0 200 OK\r\n\
         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
         CSeq: {cseq}\r\n\
         Content-Type: text/parameters\r\n\
         Content-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .context("write RTSP response")
}

fn video_latency_request(cseq: u32, snapshot: MiPlayLatencySnapshot) -> String {
    format!(
        "VIDEO_LATENCY rtsp://localhost/wfd1.0 RTSP/1.0\r\n\
         User-Agent: stagefright/1.1 (Linux;Android 4.1)\r\n\
         CSeq: {cseq}\r\n\
         Content-Type: text/parameters\r\n\
         latency:{}\r\n\
         bitrate:{}\r\n\
         rtpPacketNum:{}\r\n\
         Content-Length:0\r\n\r\n",
        snapshot.latency_millis, snapshot.bitrate_bps, snapshot.rtp_packet_number,
    )
}

fn send_video_latency_request(
    stream: &mut TcpStream,
    cseq: u32,
    snapshot: MiPlayLatencySnapshot,
) -> Result<()> {
    stream
        .write_all(video_latency_request(cseq, snapshot).as_bytes())
        .context("write MiPlay VIDEO_LATENCY feedback")
}

fn parameter_value<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    body.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        candidate
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key length");
    hmac.update(message);
    hex::encode(hmac.finalize().into_bytes())
}

fn random_hex_32() -> String {
    use rand::RngCore;
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        AdtsFramer, ClockFitPoint, MAX_RTSP_MESSAGE_BYTES, MIPLAY_RTP_CLOCK_HZ,
        MIPLAY_SAMPLE_RATE_HZ, MIPLAY_SOURCE_CHANNELS, MiPlayLatencyTelemetry, PcmQueue,
        StreamKeys, TsDemuxer, adts_bitrate, adts_sample_rate, decode_timer_sample,
        extend_wrapping_counter, fit_clock_line, is_adts, normalized_output_latency_millis,
        parse_rtp_packet, parse_timer_server, pes_private_data_iv, pes_pts_micros,
        presentation_schedule_error_micros, rtp_payload, should_hard_drop_late_frames,
        smooth_clock_model, synchronized_buffer_frames, trimmed_clock_offset,
        try_parse_rtsp_message, usable_presentation_schedule_error_micros,
        validate_miplay_sample_rate, video_latency_request,
    };
    use aes::Aes128;
    use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::NoPadding};
    use std::time::{Duration as StdDuration, Instant};

    #[test]
    fn validates_stream_key_lengths() {
        assert!(
            StreamKeys::from_strings("1234567890abcdef", "0123456789abcdef", "fedcba9876543210")
                .is_ok()
        );
        assert!(StreamKeys::from_strings("short", "0123456789abcdef", "fedcba9876543210").is_err());
    }

    #[test]
    fn rtsp_parser_preserves_pipelined_messages() {
        let mut input = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\noneRTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n"
            .to_vec();

        let first = try_parse_rtsp_message(&mut input)
            .expect("first message")
            .expect("complete first message");
        assert_eq!(first.header("cseq"), Some("1"));
        assert_eq!(first.body, b"one");

        let second = try_parse_rtsp_message(&mut input)
            .expect("second message")
            .expect("complete second message");
        assert_eq!(second.header("cseq"), Some("2"));
        assert!(second.body.is_empty());
        assert!(input.is_empty());
    }

    #[test]
    fn rtsp_parser_rejects_oversized_messages() {
        let mut oversized_header = vec![b'a'; MAX_RTSP_MESSAGE_BYTES + 1];
        assert!(try_parse_rtsp_message(&mut oversized_header).is_err());

        let mut oversized_body = format!(
            "RTSP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RTSP_MESSAGE_BYTES,
        )
        .into_bytes();
        assert!(try_parse_rtsp_message(&mut oversized_body).is_err());
    }

    #[test]
    fn reassembles_an_adts_frame_split_across_pes_payloads() {
        let frame = [0xff, 0xf9, 0x4c, 0x80, 0x01, 0x9f, 0xfc, 1, 2, 3, 4, 5];
        let mut framer = AdtsFramer::default();
        assert!(framer.push(&frame[..6]).is_empty());
        assert_eq!(framer.buffered_len(), 6);
        assert_eq!(framer.push(&frame[6..]), vec![frame.to_vec()]);
        assert_eq!(framer.buffered_len(), 0);
    }

    #[test]
    fn adts_framer_skips_noise_and_extracts_multiple_frames_in_one_scan() {
        let first = [0xff, 0xf9, 0x4c, 0x80, 0x01, 0x9f, 0xfc, 1, 2, 3, 4, 5];
        let second = [0xff, 0xf9, 0x4c, 0x80, 0x01, 0x7f, 0xfc, 6, 7, 8, 9];
        let mut input = vec![0, 1, 2, 0xff, 0];
        input.extend_from_slice(&first);
        input.extend_from_slice(&second);
        input.extend_from_slice(&[10, 11, 0xff]);

        let mut framer = AdtsFramer::default();
        assert_eq!(framer.push(&input), vec![first.to_vec(), second.to_vec()]);
        assert_eq!(framer.buffered_len(), 1);
        assert!(
            framer
                .push(&[0xf9, 0x4c, 0x80, 0x01, 0x1f, 0xfc])
                .is_empty()
        );
    }

    #[test]
    fn recognizes_adts_and_computes_bitrate() {
        let frame = [0xff, 0xf9, 0x4c, 0x80, 0x56, 0x3f, 0xfc];
        assert!(is_adts(&frame));
        assert_eq!(adts_sample_rate(&frame), Some(MIPLAY_SAMPLE_RATE_HZ));
        assert_eq!(
            validate_miplay_sample_rate(&frame).unwrap(),
            MIPLAY_SAMPLE_RATE_HZ
        );
        assert_eq!(adts_bitrate(&frame), Some(258_375));

        let mut forty_four_kilohertz = frame;
        forty_four_kilohertz[2] = 0x50;
        assert_eq!(adts_sample_rate(&forty_four_kilohertz), Some(44_100));
        assert!(validate_miplay_sample_rate(&forty_four_kilohertz).is_err());
    }

    #[test]
    fn extracts_plain_rtp_payload() {
        let packet = [
            0x80, 0xa1, 0, 1, 0, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3,
        ];
        assert_eq!(rtp_payload(&packet), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn converts_xiaomi_rtp_clock_to_microseconds() {
        let packet = [
            0x80, 0xa1, 0, 1, 0, 1, 0x5f, 0x90, 0xde, 0xad, 0xbe, 0xef, 1,
        ];
        let parsed = parse_rtp_packet(&packet).unwrap();
        assert_eq!(parsed.timestamp_micros, 1_000_000);
        assert_eq!(parsed.timestamp, MIPLAY_RTP_CLOCK_HZ);
        assert_eq!(parsed.sequence_number, 1);
    }

    #[test]
    fn extends_rtp_sequence_across_the_sixteen_bit_wrap() {
        assert_eq!(extend_wrapping_counter(Some(65_535), 0, 16), 65_536);
        assert_eq!(extend_wrapping_counter(Some(65_536), 65_535, 16), 65_535);
    }

    #[test]
    fn latency_feedback_uses_live_queue_bitrate_and_extended_rtp_number() {
        let telemetry = MiPlayLatencyTelemetry::new();
        let started = Instant::now();
        for index in 0..20_u32 {
            telemetry.observe_rtp(
                (65_530 + index) as u16,
                48_000_000_u32 + index * 1_920,
                900,
                started + StdDuration::from_micros(u64::from(index) * 21_333),
            );
        }
        telemetry.update_buffered_frames(30_000);
        telemetry.update_output_latency(20.0);

        let snapshot = telemetry.snapshot().expect("RTP telemetry");
        assert_eq!(snapshot.latency_millis, 645);
        assert_eq!(snapshot.rtp_packet_number, 65_549);
        assert!(snapshot.bitrate_bps > 300_000);
        assert!(snapshot.arrival_residual_millis.abs() < 1.0);

        let request = video_latency_request(1_234, snapshot);
        assert!(request.starts_with("VIDEO_LATENCY rtsp://localhost/wfd1.0 RTSP/1.0\r\n"));
        assert!(request.contains("CSeq: 1234\r\n"));
        assert!(request.contains("latency:645\r\n"));
        assert!(request.contains("rtpPacketNum:65549\r\n"));
        assert!(request.ends_with("Content-Length:0\r\n\r\n"));
    }

    #[test]
    fn parses_sender_timer_server_address() {
        let encoded_host = u32::from_be_bytes([192, 168, 31, 207]);
        let address = parse_timer_server(
            &format!("{encoded_host}:49152"),
            "127.0.0.1".parse().unwrap(),
        )
        .unwrap();
        assert_eq!(address.to_string(), "192.168.31.207:49152");
    }

    #[test]
    fn decodes_xiaomi_udp_timer_exchange() {
        let mut response = [0_u8; 40];
        response[16..24].copy_from_slice(&140_i64.to_le_bytes());
        response[24..32].copy_from_slice(&130_i64.to_le_bytes());
        response[32..36].copy_from_slice(&7_u32.to_le_bytes());
        assert_eq!(decode_timer_sample(&response, 7, 100, 180), Some((-5, 70)));
        assert_eq!(decode_timer_sample(&response, 8, 100, 180), None);
    }

    #[test]
    fn timer_offset_filter_rejects_network_jitter_extremes() {
        let filtered = trimmed_clock_offset(
            [
                -80_000, 998, 999, 1_000, 1_001, 1_002, 1_003, 1_004, 1_005, 90_000,
            ]
            .into_iter(),
        );
        assert_eq!(filtered, Some(1_001));
    }

    #[test]
    fn timer_line_fit_recovers_clock_frequency_drift() {
        let history = (0_i64..20)
            .map(|index| ClockFitPoint {
                local_micros: index * 5_000_000,
                offset_micros: 1_000 + index * 500,
            })
            .collect();
        let (base, offset, frequency) = fit_clock_line(&history);
        assert_eq!(base, 95_000_000);
        assert!((offset - 10_500.0).abs() < 0.001);
        assert!((frequency - 1.0001).abs() < 0.000_000_1);
    }

    #[test]
    fn timer_model_applies_initial_offset_then_smooths_updates() {
        assert_eq!(
            smooth_clock_model(false, 0, 0, 20_000, 100_000),
            (20_000, 100_000, 20_000)
        );
        assert_eq!(
            smooth_clock_model(true, 20_000, 100_000, 60_000, 200_000),
            (25_000, 125_000, 40_000),
        );
    }

    #[test]
    fn extracts_pes_presentation_timestamp() {
        let pts = 90_000_u64;
        let encoded = [
            0x20 | (((pts >> 29) as u8) & 0x0e) | 1,
            (pts >> 22) as u8,
            (((pts >> 14) as u8) & 0xfe) | 1,
            (pts >> 7) as u8,
            ((pts << 1) as u8 & 0xfe) | 1,
        ];
        let mut pes = vec![0, 0, 1, 0xc0, 0, 8, 0x80, 0x80, 5];
        pes.extend_from_slice(&encoded);
        assert_eq!(pes_pts_micros(&pes), Some(1_000_000));
    }

    #[test]
    fn ts_demuxer_prefers_pes_pts_over_the_rtp_transport_clock() {
        let pts = 90_000_u64;
        let encoded = [
            0x20 | (((pts >> 29) as u8) & 0x0e) | 1,
            (pts >> 22) as u8,
            (((pts >> 14) as u8) & 0xfe) | 1,
            (pts >> 7) as u8,
            ((pts << 1) as u8 & 0xfe) | 1,
        ];
        let elementary = [0xff, 0xf9, 0x4c, 0x80, 0x00, 0xff, 0xfc];
        let declared = 3 + encoded.len() + elementary.len();
        let mut pes = vec![
            0,
            0,
            1,
            0xc0,
            (declared >> 8) as u8,
            declared as u8,
            0x80,
            0x80,
            encoded.len() as u8,
        ];
        pes.extend_from_slice(&encoded);
        pes.extend_from_slice(&elementary);

        let mut demuxer = TsDemuxer::new(None);
        demuxer.pes = pes;
        demuxer.pes_rtp_pts_micros = Some(9_000_000);

        let decoded = demuxer.finish_pes().unwrap().unwrap();
        assert_eq!(decoded.pts_micros, Some(1_000_000));
    }

    #[test]
    fn group_sync_accounts_for_the_native_output_callback() {
        assert_eq!(synchronized_buffer_frames(20.0), 37_440);
    }

    #[test]
    fn impossible_sender_clock_schedule_falls_back_to_the_buffer_window() {
        assert_eq!(
            usable_presentation_schedule_error_micros(1_000_000.0, 800_000, 1_000_000, 0.0),
            Some(800_000.0)
        );
        assert_eq!(
            usable_presentation_schedule_error_micros(1_000_000.0, 2_000_000, 1_000_000, 0.0),
            None
        );
        assert_eq!(
            usable_presentation_schedule_error_micros(f64::NAN, 0, 0, 0.0),
            None
        );
    }

    #[test]
    fn android_latency_uses_endpoint_measurement_or_callback_fallback() {
        assert_eq!(
            normalized_output_latency_millis(Some(20.0), 48_000, 240),
            20.0
        );
        assert_eq!(normalized_output_latency_millis(None, 48_000, 240), 5.0);
        assert_eq!(
            normalized_output_latency_millis(Some(400.0), 48_000, 240),
            250.0
        );
    }

    #[test]
    fn grouped_receivers_align_despite_different_endpoint_latency() {
        let first = presentation_schedule_error_micros(1_000_000.0, 800_000, 1_770_000, 20.0);
        let second = presentation_schedule_error_micros(1_000_000.0, 800_000, 1_775_000, 15.0);
        assert_eq!(first, 10_000.0);
        assert_eq!(second, first);
    }

    #[test]
    fn android_sync_never_hard_drops_after_audio_is_running() {
        assert!(should_hard_drop_late_frames(false, -3_000.0, false));
        assert!(!should_hard_drop_late_frames(true, -500_000.0, false));
        assert!(should_hard_drop_late_frames(true, -26_000.0, true));
    }

    #[test]
    fn pcm_queue_waits_for_the_group_playout_window() {
        let target_frames = 1_000;
        let mut queue = PcmQueue::new();
        queue.push(&vec![0.0; (target_frames - 1) * MIPLAY_SOURCE_CHANNELS]);
        assert!(!queue.ready_to_play(target_frames));
        queue.push(&[0.0; MIPLAY_SOURCE_CHANNELS]);
        assert!(queue.ready_to_play(target_frames));
        queue.clear();
        assert!(!queue.ready_to_play(target_frames));
    }

    #[test]
    fn pcm_queue_rate_controller_stays_within_quarter_percent() {
        let target_frames = 10_000;
        let nominal = 1.0;
        let mut queue = PcmQueue::new();
        queue.push(&vec![0.0; (target_frames + 480) * MIPLAY_SOURCE_CHANNELS]);
        let faster = queue.playback_rate(target_frames, nominal);
        assert!(faster > nominal);
        assert!(faster <= nominal * 1.0025);

        queue.clear();
        queue.push(&vec![0.0; (target_frames - 480) * MIPLAY_SOURCE_CHANNELS]);
        let slower = queue.playback_rate(target_frames, nominal);
        assert!(slower < nominal);
        assert!(slower >= nominal * 0.9975);
    }

    #[test]
    fn pcm_queue_tracks_timestamp_when_late_frames_are_dropped() {
        let mut queue = PcmQueue::new();
        queue.push_timed(&vec![0.0; 100 * MIPLAY_SOURCE_CHANNELS], Some(1_000.0));
        assert_eq!(queue.drop_frames(48), 48);
        assert!((queue.front_pts_micros().unwrap() - 2_000.0).abs() < 0.001);
    }

    #[test]
    fn pcm_queue_renders_an_android_callback_as_one_block() {
        let mut queue = PcmQueue::new();
        queue.push_timed(
            &[
                0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0, 4.0, 14.0, 5.0, 15.0,
            ],
            Some(1_000.0),
        );
        let mut output = [(0.0, 0.0); 3];

        let peak = queue.render_stereo_block(&mut output, 1.0);

        assert_eq!(output, [(0.0, 10.0), (1.0, 11.0), (2.0, 12.0)]);
        assert_eq!(peak, 12.0);
        assert_eq!(queue.buffered_frames(), 3);
        assert!(
            (queue.front_pts_micros().unwrap() - 1_062.5).abs() < 0.001,
            "block consumption must advance the queue timestamp once"
        );
    }

    #[test]
    fn ts_demuxer_ignores_non_audio_pid() {
        let mut demuxer = TsDemuxer::new(Some(StreamKeys {
            auth_key: *b"auth-key-16bytes",
            stream_key: *b"0123456789abcdef",
            stream_iv: *b"fedcba9876543210",
        }));
        let mut packet = [0xff_u8; 188];
        packet[..4].copy_from_slice(&[0x47, 0x40, 0x00, 0x10]);
        assert!(demuxer.push_ts(&packet).unwrap().is_empty());
    }

    #[test]
    fn reads_miplay_iv_from_pes_private_data() {
        let iv = *b"private-pes-iv!!";
        let mut pes = vec![0, 0, 1, 0xc0, 0, 0, 0x80, 0x01, 17, 0x80];
        pes.extend_from_slice(&iv);
        pes.extend_from_slice(&[0; 32]);
        assert_eq!(pes_private_data_iv(&pes, 26), Some(iv));
    }

    #[test]
    fn decrypts_only_miplay_pes_prefix_with_private_iv() {
        let key = *b"0123456789abcdef";
        let initial_iv = *b"fedcba9876543210";
        let private_iv = *b"private-pes-iv!!";
        let mut elementary = vec![0_u8; 320];
        elementary[..7].copy_from_slice(&[0xff, 0xf9, 0x4c, 0x80, 0x28, 0x1f, 0xfc]);
        for (index, byte) in elementary[7..].iter_mut().enumerate() {
            *byte = index as u8;
        }
        let clear_tail = elementary[256..].to_vec();
        cbc::Encryptor::<Aes128>::new_from_slices(&key, &private_iv)
            .unwrap()
            .encrypt_padded::<NoPadding>(&mut elementary[..256], 256)
            .unwrap();

        let declared = 3 + 17 + elementary.len();
        let mut pes = vec![
            0,
            0,
            1,
            0xc0,
            (declared >> 8) as u8,
            declared as u8,
            0x80,
            0x01,
            17,
            0x80,
        ];
        pes.extend_from_slice(&private_iv);
        pes.extend_from_slice(&elementary);

        let mut demuxer = TsDemuxer::new(Some(StreamKeys {
            auth_key: *b"auth-key-16bytes",
            stream_key: key,
            stream_iv: initial_iv,
        }));
        demuxer.pes = pes;
        let decrypted = demuxer.finish_pes().unwrap().unwrap();
        assert!(is_adts(&decrypted.payload));
        assert_eq!(&decrypted.payload[256..], clear_tail.as_slice());
        assert_eq!(demuxer.keys.unwrap().stream_iv, private_iv);
    }

    #[test]
    fn accepts_plain_tv_pes_without_stream_keys() {
        let elementary = vec![0xff, 0xf9, 0x4c, 0x80, 0x00, 0xff, 0xfc];
        let declared = 3 + elementary.len();
        let mut pes = vec![
            0,
            0,
            1,
            0xc0,
            (declared >> 8) as u8,
            declared as u8,
            0x80,
            0x00,
            0,
        ];
        pes.extend_from_slice(&elementary);

        let mut demuxer = TsDemuxer::new(None);
        demuxer.pes = pes;
        let decoded = demuxer.finish_pes().unwrap().unwrap();
        assert_eq!(decoded.payload, elementary);
        assert_eq!(decoded.pts_micros, None);
    }
}
