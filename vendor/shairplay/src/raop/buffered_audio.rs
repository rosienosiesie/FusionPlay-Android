//! AirPlay 2 buffered audio processor (stream type 103).
//!
//! Receives encrypted AAC packets over TCP, decrypts with ChaCha20-Poly1305,
//! decodes via symphonia, resamples/mixes down, and delivers F32LE PCM through
//! a timed playout buffer.
//!
//! Three concurrent tasks:
//! - **Receiver** (tokio): accepts TCP, decrypts, decodes, buffers by RTP timestamp
//! - **Command handler** (tokio): processes SetRate/Flush/Stop from RTSP thread
//! - **Delivery** (std::thread): timed playout using anchor-based scheduling

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::codec::aac::{AacDecoder, AudioSsrc};
use crate::error::{CodecError, NetworkError, ShairplayError};
use crate::net::ptp::PtpClock;
use crate::raop::audio_pipeline::{NONCE_TRAIL_LEN, RTP_HEADER_LEN, decrypt_rtp_chacha};
use crate::raop::{AudioCodec, AudioFormat, AudioHandler, SourceAudioCodec, SourceAudioFormat};
use crate::util::now_ns;

#[derive(Debug, Clone)]
/// Output configuration passed from the server builder.
pub(crate) struct OutputConfig {
    /// Target sample rate, or None for source native rate.
    pub(crate) sample_rate: Option<u32>,
    /// Maximum output channels, or None to pass through.
    pub(crate) max_channels: Option<u8>,
}

#[derive(Debug)]
/// Commands sent from the RTSP handler thread to the playout engine.
pub enum PlayoutCommand {
    /// Set playback rate and anchor point. rate=0 means pause.
    SetRate {
        /// RTP timestamp at the anchor point.
        anchor_rtp: u32,
        /// Network time at the anchor point (ns).
        anchor_time_ns: u64,
        /// PTP timeline identity for `anchor_time_ns`, or zero if omitted.
        anchor_clock_id: u64,
        /// Playback rate (1 = playing, 0 = paused).
        rate: u32,
    },
    /// Pause playout while preserving the current anchor and buffered stream.
    Pause,
    /// Resume the existing stream, re-anchoring to the earliest buffered frame.
    Resume,
    /// Flush buffered frames in the given RTP timestamp range.
    Flush {
        /// First timestamp to flush.
        from_seq: u32,
        /// Last timestamp to flush.
        until_seq: u32,
    },
    /// Stop playback and tear down.
    Stop,
}

struct PlayoutState {
    buffer: BTreeMap<u32, Vec<f32>>, // rtp_timestamp → F32 PCM samples
    anchor_rtp: u32,
    anchor_local_ns: u64,
    anchor_valid: bool,
    rate: u32,
    rtp_clock_rate: u32,
    sample_rate: u32,
    channels: u8,
    source_format: Option<SourceAudioFormat>,
    stopped: bool,
    format_changed: bool,
}

fn pause_playout(state: &mut PlayoutState) {
    state.rate = 0;
}

fn resume_playout(state: &mut PlayoutState) {
    state.rate = 1;
    if let Some(&first_ts) = state.buffer.keys().next() {
        let lead_frames = state.rtp_clock_rate / 10;
        state.anchor_rtp = first_ts.wrapping_sub(lead_frames);
        state.anchor_local_ns = now_ns();
        state.anchor_valid = true;
    } else {
        // The next received frame will establish the anchor.
        state.anchor_valid = false;
    }
}

fn anchor_first_resumed_packet(state: &mut PlayoutState, timestamp: u32) {
    if state.rate == 0 || state.anchor_valid {
        return;
    }
    let lead_frames = state.rtp_clock_rate / 10;
    state.anchor_rtp = timestamp.wrapping_sub(lead_frames);
    state.anchor_local_ns = now_ns();
    state.anchor_valid = true;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorMapping {
    Ptp,
    LocalFallback,
}

fn set_rate_anchor(
    state: &mut PlayoutState,
    ptp_clock: &PtpClock,
    anchor_rtp: u32,
    anchor_time_ns: u64,
    anchor_clock_id: u64,
    rate: u32,
    local_now_ns: u64,
) -> (AnchorMapping, usize) {
    state.rate = rate;
    if rate == 0 {
        return (AnchorMapping::LocalFallback, 0);
    }

    // A sender-provided RTP anchor is authoritative, especially after a seek.
    // Synthetic resume commands use zero and fall back to the earliest buffered
    // frame. The network timestamp is meaningful only with a live matching PTP
    // mapping; otherwise retain the previous best-effort local behavior.
    if anchor_rtp != 0 {
        state.anchor_rtp = anchor_rtp;
        state.anchor_valid = true;
    } else if let Some(&first_ts) = state.buffer.keys().next() {
        let lead_frames = state.rtp_clock_rate / 10;
        state.anchor_rtp = first_ts.wrapping_sub(lead_frames);
        state.anchor_valid = true;
    } else {
        // SETRATEANCHORTIME can arrive before the first buffered packet. Let
        // packet insertion establish an anchor instead of comparing a random
        // RTP timestamp against zero.
        state.anchor_rtp = anchor_rtp;
        state.anchor_valid = false;
    }

    let mapped_local_time = (anchor_rtp != 0 && anchor_time_ns != 0)
        .then(|| ptp_clock.local_time_for_master_time_at(anchor_time_ns, anchor_clock_id, local_now_ns))
        .flatten();
    let mapping = if let Some(anchor_local_ns) = mapped_local_time {
        state.anchor_local_ns = anchor_local_ns;
        AnchorMapping::Ptp
    } else {
        state.anchor_local_ns = local_now_ns;
        AnchorMapping::LocalFallback
    };

    let stale: Vec<u32> = state
        .buffer
        .keys()
        .filter(|&&ts| (state.anchor_rtp.wrapping_sub(ts) as i32) > 0)
        .copied()
        .collect();
    let discarded = stale.len();
    for timestamp in stale {
        state.buffer.remove(&timestamp);
    }

    (mapping, discarded)
}

fn playout_target_rtp(state: &PlayoutState, local_now_ns: u64) -> Option<u32> {
    if !state.anchor_valid || local_now_ns < state.anchor_local_ns {
        return None;
    }

    let elapsed_ns = local_now_ns - state.anchor_local_ns;
    let elapsed_frames = (elapsed_ns as u128 * state.rtp_clock_rate as u128 / 1_000_000_000) as u32;
    Some(state.anchor_rtp.wrapping_add(elapsed_frames))
}

/// TCP listener for buffered audio. Binds a port and spawns the processing pipeline.
pub(crate) struct BufferedAudioProcessor {
    /// TCP listener waiting for the iPhone to connect.
    pub(crate) listener: TcpListener,
    /// Server-wide mapping from the sender's PTP clock to local time.
    pub(crate) ptp_clock: PtpClock,
}

impl BufferedAudioProcessor {
    /// Start the processing pipeline. Returns a command sender for playback control.
    pub(crate) fn start(
        self,
        shk: [u8; 32],
        output_config: OutputConfig,
        handler: Arc<dyn AudioHandler>,
    ) -> tokio::sync::mpsc::UnboundedSender<PlayoutCommand> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let default_sr = output_config.sample_rate.unwrap_or(44100);

        let state = Arc::new((
            Mutex::new(PlayoutState {
                buffer: BTreeMap::new(),
                anchor_rtp: 0,
                anchor_local_ns: 0,
                anchor_valid: false,
                rate: 0,
                rtp_clock_rate: 44_100,
                sample_rate: default_sr,
                channels: 2,
                source_format: None,
                stopped: false,
                format_changed: false,
            }),
            Condvar::new(),
        ));

        // Delivery thread
        let state2 = state.clone();
        let handler2 = handler.clone();
        let output_config2 = output_config.clone();
        std::thread::spawn(move || {
            delivery_loop(state2, handler2, output_config2);
        });

        // Command handler
        let state3 = state.clone();
        let ptp_clock = self.ptp_clock.clone();
        let mut cmd_rx = cmd_rx;
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let (lock, cvar) = &*state3;
                let mut s = lock.lock().unwrap();
                match cmd {
                    PlayoutCommand::SetRate {
                        anchor_rtp,
                        anchor_time_ns,
                        anchor_clock_id,
                        rate,
                    } => {
                        let was_paused = s.rate == 0;
                        if rate == 0 {
                            s.rate = 0;
                            info!("Playout paused");
                        } else {
                            let (mapping, discarded) = set_rate_anchor(
                                &mut s,
                                &ptp_clock,
                                anchor_rtp,
                                anchor_time_ns,
                                anchor_clock_id,
                                rate,
                                now_ns(),
                            );
                            if discarded != 0 {
                                debug!(discarded, "Discarded stale frames");
                            }
                            if was_paused {
                                info!(
                                    anchor_rtp,
                                    anchor_local_ns = s.anchor_local_ns,
                                    ptp_synchronized = mapping == AnchorMapping::Ptp,
                                    "Playout started"
                                );
                            }
                        }
                        cvar.notify_all();
                    }
                    PlayoutCommand::Pause => {
                        // A plain RTSP PAUSE does not carry a new RTP anchor.
                        // Preserve the old one and, critically, keep the stream
                        // and decoder alive for the matching RECORD.
                        pause_playout(&mut s);
                        info!("Playout paused");
                        cvar.notify_all();
                    }
                    PlayoutCommand::Resume => {
                        resume_playout(&mut s);
                        info!("Playout resumed");
                        cvar.notify_all();
                    }
                    PlayoutCommand::Flush { from_seq, until_seq } => {
                        let keys: Vec<u32> = s
                            .buffer
                            .keys()
                            .filter(|&&ts| ts >= from_seq && ts <= until_seq)
                            .copied()
                            .collect();
                        for k in &keys {
                            s.buffer.remove(k);
                        }
                        debug!(flushed = keys.len(), "Flushed");
                    }
                    PlayoutCommand::Stop => {
                        s.stopped = true;
                        s.buffer.clear();
                        cvar.notify_all();
                        break;
                    }
                }
            }
        });

        // Receiver task. A sender may close and reopen only the buffered-audio
        // TCP socket when pausing. Keep accepting sequential data connections
        // until the RTSP stream is explicitly stopped.
        let state4 = state.clone();

        tokio::spawn(async move {
            loop {
                if state4.0.lock().is_ok_and(|state| state.stopped) {
                    break;
                }

                let (stream, addr) = match self.listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("Buffered audio accept failed: {e}");
                        handler.on_error(&ShairplayError::Network(NetworkError::Io(e)));
                        return;
                    }
                };
                if state4.0.lock().is_ok_and(|state| state.stopped) {
                    break;
                }

                info!(%addr, "Buffered audio client connected");
                receive_loop(stream, &shk, output_config.clone(), state4.clone(), &handler).await;

                if state4.0.lock().is_ok_and(|state| state.stopped) {
                    break;
                }
                info!("Buffered audio client disconnected; waiting for stream resume");
            }
        });

        cmd_tx
    }
}

/// TCP receive loop: reads length-prefixed packets, decrypts, decodes, buffers.
async fn receive_loop(
    mut stream: TcpStream,
    shk: &[u8; 32],
    output_config: OutputConfig,
    state: Arc<(Mutex<PlayoutState>, Condvar)>,
    handler: &Arc<dyn AudioHandler>,
) {
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit};

    let cipher = ChaCha20Poly1305::new(shk.into());
    let mut len_buf = [0u8; 2];
    let mut decoder: Option<AacDecoder> = None;
    let mut current_ssrc = AudioSsrc::None;
    let mut stream_resampler: Option<crate::codec::resample::StreamResampler> = None;
    let mut source_channels: u8 = 2;
    let mut output_channels: u8 = 2;

    loop {
        if state.0.lock().is_ok_and(|state| state.stopped) {
            break;
        }
        if stream.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let total_len = u16::from_be_bytes(len_buf) as usize;
        if total_len < 2 {
            break;
        }

        let mut packet = vec![0u8; total_len - 2];
        if stream.read_exact(&mut packet).await.is_err() {
            break;
        }
        if packet.len() <= RTP_HEADER_LEN + NONCE_TRAIL_LEN {
            continue;
        }

        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let ssrc_val = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let ssrc = AudioSsrc::from_u32(ssrc_val);

        // Detect format change
        if ssrc != current_ssrc
            && let (Some(src_sr), Some(src_ch)) = (ssrc.sample_rate(), ssrc.channels())
        {
            current_ssrc = ssrc;
            info!(ssrc = ?ssrc, src_sr, src_ch, "Audio format detected");

            decoder = AacDecoder::new(src_sr, src_ch).ok();
            if decoder.is_none() {
                warn!("Failed to create AAC decoder for {:?}", ssrc);
                handler.on_error(&ShairplayError::Codec(CodecError::UnsupportedFormat(format!(
                    "AAC decoder init failed (ssrc={ssrc:?}, sample_rate={src_sr}, channels={src_ch})"
                ))));
            }

            let target_sr = output_config.sample_rate.unwrap_or(src_sr);
            let target_ch = output_config.max_channels.map(|max| src_ch.min(max)).unwrap_or(src_ch);

            stream_resampler = crate::codec::resample::StreamResampler::new(src_sr, target_sr, target_ch as usize);
            if stream_resampler.is_some() {
                debug!(from = src_sr, to = target_sr, "Resampler initialized");
            }

            source_channels = src_ch;
            output_channels = target_ch;

            // Signal format change to delivery thread
            let (lock, cvar) = &*state;
            let mut s = lock.lock().unwrap();
            let source_format = SourceAudioFormat {
                codec: if ssrc.is_alac() {
                    SourceAudioCodec::Alac
                } else {
                    SourceAudioCodec::Aac
                },
                bits: ssrc.bit_depth(),
                channels: Some(src_ch),
                sample_rate: Some(src_sr),
            };
            let format_changed =
                s.sample_rate != target_sr || s.channels != target_ch || s.source_format != Some(source_format);
            // RTP timestamps advance in the source clock domain even when PCM
            // is resampled to a different output rate.
            s.rtp_clock_rate = src_sr;
            s.sample_rate = target_sr;
            s.channels = target_ch;
            s.source_format = Some(source_format);
            s.format_changed |= format_changed;
            cvar.notify_all();
        }

        // Decrypt the ChaCha20-Poly1305 RTP frame.
        let Some(plaintext) = decrypt_rtp_chacha(&cipher, &packet) else {
            debug!("Audio decrypt failed");
            continue;
        };

        // Decode
        let pcm = if let Some(dec) = &mut decoder {
            dec.decode(&plaintext)
        } else {
            None
        };

        if let Some(pcm_data) = pcm {
            // Convert bytes to f32 samples for processing
            let samples: Vec<f32> = pcm_data
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();

            // Mix down + resample to the output format.
            let samples = crate::codec::resample::mixdown_and_resample(
                samples,
                source_channels,
                output_channels,
                &mut stream_resampler,
            );

            let (lock, cvar) = &*state;
            let mut s = lock.lock().unwrap();
            s.buffer.insert(timestamp, samples);
            anchor_first_resumed_packet(&mut s, timestamp);
            cvar.notify_all();
        }
    }
    debug!("Buffered audio receive loop ended");
}

/// Timed playout delivery thread. Wakes on condvar, delivers due frames to AudioSession.
fn delivery_loop(
    state: Arc<(Mutex<PlayoutState>, Condvar)>,
    handler: Arc<dyn AudioHandler>,
    _output_config: OutputConfig,
) {
    let (lock, cvar) = &*state;
    let mut session: Option<Box<dyn crate::raop::AudioSession>> = None;

    loop {
        let mut s = lock.lock().unwrap();

        while !s.stopped && (s.rate == 0 || !s.anchor_valid || s.buffer.is_empty()) {
            s = cvar.wait(s).unwrap();
        }
        if s.stopped {
            break;
        }

        // Lazy init or reinit session on format change
        if session.is_none() || s.format_changed {
            s.format_changed = false;
            let format = AudioFormat {
                codec: AudioCodec::Pcm,
                bits: 32,
                channels: s.channels,
                sample_rate: s.sample_rate,
                source: s.source_format,
            };
            info!(?format, "Audio session initialized");
            // Emit the previous session's stop before the replacement session
            // announces its start. Reconnected streams with the same format do
            // not take this path and therefore retain their session.
            drop(session.take());
            session = Some(handler.audio_init(format));
        }

        let target_rtp = playout_target_rtp(&s, now_ns());
        let ready: Vec<(u32, Vec<f32>)> = target_rtp
            .map(|target_rtp| {
                s.buffer
                    .iter()
                    .filter(|(ts, _)| (target_rtp.wrapping_sub(**ts) as i32) >= 0)
                    .map(|(&ts, data)| (ts, data.clone()))
                    .collect()
            })
            .unwrap_or_default();

        for (ts, _) in &ready {
            s.buffer.remove(ts);
        }
        drop(s);

        if let Some(ref mut sess) = session {
            for (_, frame) in &ready {
                sess.audio_process(frame);
            }
        }

        if ready.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    info!("Delivery loop ended");
}

#[cfg(test)]
mod tests {
    use super::{
        AnchorMapping, PlayoutState, anchor_first_resumed_packet, pause_playout, playout_target_rtp, resume_playout,
        set_rate_anchor,
    };
    use crate::net::ptp::PtpClock;
    use std::collections::BTreeMap;

    fn state() -> PlayoutState {
        PlayoutState {
            buffer: BTreeMap::new(),
            anchor_rtp: 123_456,
            anchor_local_ns: 42,
            anchor_valid: true,
            rate: 1,
            rtp_clock_rate: 48_000,
            sample_rate: 48_000,
            channels: 2,
            source_format: None,
            stopped: false,
            format_changed: false,
        }
    }

    #[test]
    fn pause_preserves_anchor_and_buffer_for_resume() {
        let mut state = state();
        state.buffer.insert(130_000, vec![0.25, -0.25]);

        pause_playout(&mut state);

        assert_eq!(state.rate, 0);
        assert_eq!(state.anchor_rtp, 123_456);
        assert!(state.anchor_valid);
        assert_eq!(state.buffer.len(), 1);
    }

    #[test]
    fn record_resume_waits_for_and_anchors_the_next_packet() {
        let mut state = state();
        pause_playout(&mut state);
        resume_playout(&mut state);

        assert_eq!(state.rate, 1);
        assert!(!state.anchor_valid);

        anchor_first_resumed_packet(&mut state, 240_000);

        assert!(state.anchor_valid);
        assert_eq!(state.anchor_rtp, 235_200);
    }

    #[test]
    fn ptp_anchor_maps_master_time_to_local_time() {
        let mut state = state();
        let clock = PtpClock::new();
        let local_now = 10_000_000_000;
        let local_to_master_offset = 90_000_000_000;
        clock.update(0x1122, local_now, local_to_master_offset, local_now);

        let (mapping, discarded) = set_rate_anchor(&mut state, &clock, 240_000, 100_500_000_000, 0x1122, 1, local_now);

        assert_eq!(mapping, AnchorMapping::Ptp);
        assert_eq!(discarded, 0);
        assert_eq!(state.anchor_local_ns, 10_500_000_000);
    }

    #[test]
    fn future_ptp_anchor_is_not_due_until_local_anchor_time() {
        let mut state = state();
        state.anchor_rtp = 240_000;
        state.anchor_local_ns = 10_500_000_000;
        state.anchor_valid = true;

        assert_eq!(playout_target_rtp(&state, 10_499_999_999), None);
        assert_eq!(playout_target_rtp(&state, 10_500_000_000), Some(240_000));
        assert_eq!(playout_target_rtp(&state, 11_500_000_000), Some(288_000));
    }

    #[test]
    fn missing_ptp_clock_keeps_local_fallback() {
        let mut state = state();
        let local_now = 10_000_000_000;

        let (mapping, _) = set_rate_anchor(
            &mut state,
            &PtpClock::new(),
            240_000,
            100_500_000_000,
            0x1122,
            1,
            local_now,
        );

        assert_eq!(mapping, AnchorMapping::LocalFallback);
        assert_eq!(state.anchor_local_ns, local_now);
        assert_eq!(playout_target_rtp(&state, local_now), Some(240_000));
    }

    #[test]
    fn playout_uses_source_rtp_clock_rate_after_resampling() {
        let mut state = state();
        state.sample_rate = 96_000;
        state.rtp_clock_rate = 48_000;
        state.anchor_rtp = 240_000;
        state.anchor_local_ns = 10_000_000_000;

        assert_eq!(playout_target_rtp(&state, 11_000_000_000), Some(288_000));
    }
}
