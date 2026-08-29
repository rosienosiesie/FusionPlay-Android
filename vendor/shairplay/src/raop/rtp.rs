//! AP1 RTP audio streaming — UDP and TCP receiver with ALAC decode.
//!
//! Manages the full AP1 audio receive pipeline:
//!
//! ```text
//! iPhone → RTP/UDP or RTP/TCP → RaopRtp → RaopBuffer (decrypt+decode) → AudioSession
//! ```
//!
//! Two transport modes:
//! - **UDP** (default): data, control, and timing on separate UDP sockets.
//!   Control channel carries retransmit responses (payload type 0x56).
//! - **TCP**: single TCP connection with `$`-prefixed interleaved framing.
//!   No retransmits (reliable transport).

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Mutex, watch};
use tracing::info;

use crate::codec::alac::AlacConfig;
use crate::error::{NetworkError, ShairplayError};
use crate::raop::buffer::{RAOP_PACKET_LEN, RaopBuffer};
use crate::raop::{AudioCodec, AudioFormat, AudioHandler, SourceAudioCodec, SourceAudioFormat};

/// Sentinel value for [`RtpState::flush`] indicating no flush is pending.
const NO_FLUSH: i32 = -42;

/// RTP payload type for retransmit (RESEND) responses on the control channel.
const CTRL_PAYLOAD_TYPE: u8 = 0x56;
/// RTP payload type used by classic AirPlay to map its media clock to NTP.
const CTRL_SYNC_PAYLOAD_TYPE: u8 = 0x54;
/// Bytes of retransmit header preceding the original RTP packet in a RESEND.
const RETRANSMIT_HEADER_LEN: usize = 4;
const RESEND_FIRST_CHECK_DELAY: Duration = Duration::from_millis(100);
const RESEND_REPEAT_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RESEND_RUNS_PER_CHECK: usize = 8;
/// Sync packets contain an 8-byte RTP header, an 8-byte NTP timestamp, and the
/// RTP timestamp of the next packet.
const SYNC_PACKET_LEN: usize = 20;
/// Compatibility fallback for non-conforming senders that never emit sync.
///
/// A conforming sender emits sync once per second. Holding its first audio for
/// up to 1.5 seconds prevents the normal AirPlay look-ahead from being played
/// immediately, while still allowing unusual senders to work.
const INITIAL_SYNC_WAIT_NS: u64 = 1_500_000_000;

fn local_clock_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

/// Build Apple's non-standard RTCP resend request packet.
fn resend_request(first_sequence: u16, count: u16) -> [u8; 8] {
    let first = first_sequence.to_be_bytes();
    let count = count.to_be_bytes();
    [0x80, 0xd5, 0x00, 0x01, first[0], first[1], count[0], count[1]]
}

#[derive(Debug, Clone, Copy, Default)]
struct ClassicPlayoutClock {
    anchor_rtp: u32,
    anchor_ntp: u64,
    anchor_local_ns: u64,
    synchronized: bool,
    reanchor_on_next_sync: bool,
    fallback_anchor_rtp: Option<u32>,
    first_audio_local_ns: Option<u64>,
}

impl ClassicPlayoutClock {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn observe_sync(&mut self, packet: &[u8], received_local_ns: u64) -> bool {
        if packet.len() < SYNC_PACKET_LEN || packet[0] >> 6 != 2 || packet[1] & 0x7f != CTRL_SYNC_PAYLOAD_TYPE {
            return false;
        }

        // The timestamp in the shortened RTP header is the sample being
        // presented at the packet's NTP time. Packets received on a LAN are
        // only a few milliseconds behind that instant, so receipt time gives
        // a stable local anchor without depending on the wall clocks having
        // identical epochs. Bytes 16..20 describe the next (future) audio
        // packet and must not be used as the current playout point.
        let current_rtp = u32::from_be_bytes(packet[4..8].try_into().expect("checked sync packet length"));
        let current_ntp = u64::from_be_bytes(packet[8..16].try_into().expect("checked sync packet length"));
        let projected_local_ns = if self.synchronized && !self.reanchor_on_next_sync {
            let delta_ns = ntp_delta_ns(self.anchor_ntp, current_ntp);
            if delta_ns <= 0 {
                // Ignore duplicated or reordered sync packets instead of
                // moving the playout clock backwards.
                return false;
            }
            (i128::from(self.anchor_local_ns) + delta_ns).clamp(0, i128::from(u64::MAX)) as u64
        } else {
            received_local_ns
        };
        self.anchor_rtp = current_rtp;
        self.anchor_ntp = current_ntp;
        self.anchor_local_ns = projected_local_ns;
        self.synchronized = true;
        self.reanchor_on_next_sync = false;
        self.fallback_anchor_rtp = None;
        self.first_audio_local_ns = None;
        true
    }

    fn shift_local_timeline(&mut self, paused_ns: u64) {
        if self.synchronized {
            self.anchor_local_ns = self.anchor_local_ns.saturating_add(paused_ns);
            self.reanchor_on_next_sync = true;
        }
        if let Some(first_audio_local_ns) = self.first_audio_local_ns.as_mut() {
            *first_audio_local_ns = first_audio_local_ns.saturating_add(paused_ns);
        }
    }

    fn frame_is_due(&mut self, timestamp: u32, sample_rate: u32, local_now_ns: u64) -> bool {
        if sample_rate == 0 {
            return true;
        }
        if !self.synchronized {
            let first_rtp = *self.fallback_anchor_rtp.get_or_insert(timestamp);
            let first = *self.first_audio_local_ns.get_or_insert(local_now_ns);
            let frame_delta = timestamp.wrapping_sub(first_rtp) as i32 as i128;
            let target_delta_ns = frame_delta * 1_000_000_000_i128 / i128::from(sample_rate);
            let target_local_ns = i128::from(first) + i128::from(INITIAL_SYNC_WAIT_NS) + target_delta_ns;
            return i128::from(local_now_ns) >= target_local_ns;
        }

        let frame_delta = timestamp.wrapping_sub(self.anchor_rtp) as i32 as i128;
        let target_delta_ns = frame_delta * 1_000_000_000_i128 / i128::from(sample_rate);
        let target_local_ns = i128::from(self.anchor_local_ns) + target_delta_ns;
        i128::from(local_now_ns) >= target_local_ns
    }
}

fn ntp_delta_ns(previous: u64, current: u64) -> i128 {
    let previous_seconds = (previous >> 32) as u32;
    let current_seconds = (current >> 32) as u32;
    let seconds_delta = i128::from(current_seconds.wrapping_sub(previous_seconds) as i32);
    let fraction_delta = i128::from(current as u32) - i128::from(previous as u32);
    seconds_delta * 1_000_000_000_i128 + fraction_delta * 1_000_000_000_i128 / (1_i128 << 32)
}

/// Determine the bind address for RTP sockets.
/// Uses the specific local IP for routable addresses (respects BindConfig).
/// Falls back to unspecified for link-local IPv6 — the iPhone may send RTP
/// packets from a different address than the RTSP connection used.
fn rtp_bind_addr(local: IpAddr) -> IpAddr {
    match local {
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        other => other,
    }
}

fn bind_udp(addr: SocketAddr) -> Result<UdpSocket, ShairplayError> {
    let socket = std::net::UdpSocket::bind(addr).map_err(NetworkError::Io)?;
    socket.set_nonblocking(true).map_err(NetworkError::Io)?;
    UdpSocket::from_std(socket)
        .map_err(NetworkError::Io)
        .map_err(Into::into)
}

fn bind_tcp(addr: SocketAddr) -> Result<TcpListener, ShairplayError> {
    let listener = std::net::TcpListener::bind(addr).map_err(NetworkError::Io)?;
    listener.set_nonblocking(true).map_err(NetworkError::Io)?;
    TcpListener::from_std(listener)
        .map_err(NetworkError::Io)
        .map_err(Into::into)
}

/// Parse the SDP `c=` remote address to raw IP bytes for DACP callbacks.
/// Handles "IP6 ::1" prefix and IPv4-mapped IPv6 addresses.
pub(crate) fn remote_addr_bytes(remote: &str) -> Vec<u8> {
    let addr_str = remote.strip_prefix("IP6 ").unwrap_or(remote);
    if let Ok(ip) = addr_str.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => v4.octets().to_vec(),
            IpAddr::V6(v6) => v6.octets().to_vec(),
        }
    } else {
        vec![]
    }
}

/// Mutable state shared between the RTP receive loop and the RTSP handler thread.
/// Updated via async message passing (tokio Mutex), consumed in the receive loop.
struct RtpState {
    /// Current volume in dB (0.0 = max, -144.0 = mute).
    /// Set to true when volume changes; cleared after delivery.
    /// Pending DMAP track metadata (binary).
    /// Pending album artwork (JPEG/PNG).
    /// DACP ID for remote control discovery.
    /// Active-Remote token for DACP authentication.
    /// Pending playback progress (start, current, end in RTP timestamps).
    /// Sequence number to flush to, or [`NO_FLUSH`] if no flush pending.
    flush: i32,
    /// Sender-clock mapping used to present classic RAOP frames at their RTP
    /// timestamp instead of immediately upon network arrival.
    playout_clock: ClassicPlayoutClock,
}

/// Configuration for creating an AP1 RTP session, parsed from SDP.
pub(crate) struct RtpConfig {
    /// SDP `c=` remote address string (e.g. "192.168.1.5").
    pub(crate) remote: String,
    /// Local IP address to bind sockets to.
    pub(crate) local_addr: IpAddr,
    /// SDP `a=rtpmap` attribute.
    pub(crate) rtpmap: String,
    /// SDP `a=fmtp` attribute (ALAC configuration).
    pub(crate) fmtp: String,
    /// 128-bit AES session key (decrypted from SDP).
    pub(crate) aes_key: [u8; 16],
    /// 128-bit AES initialization vector.
    pub(crate) aes_iv: [u8; 16],
    /// If set, resample decoded audio to this rate.
    pub(crate) output_sample_rate: Option<u32>,
    /// Full socket address of the remote peer (preserves scope_id for link-local IPv6).
    pub(crate) remote_socket: std::net::SocketAddr,
}

/// AP1 RTP streaming session.
///
/// Owns the UDP/TCP sockets, the packet buffer, and the ALAC decoder.
/// Created when the iPhone sends the SDP ANNOUNCE. Started during RTSP SETUP,
/// which binds ports and spawns the receive task.
///
/// Dropped when the RTSP connection closes, which sends a shutdown signal
/// to the receive task via the [`watch`] channel.
pub(crate) struct RaopRtp {
    handler: Arc<dyn AudioHandler>,
    /// SDP `c=` remote address string (e.g. "192.168.1.5").
    remote: String,
    /// Local IP address to bind sockets to (matches the RTSP connection's interface).
    local_addr: IpAddr,
    /// If set, resample decoded audio to this rate before delivery.
    output_sample_rate: Option<u32>,
    /// Parsed ALAC stream configuration.
    config: AlacConfig,
    /// Shared packet buffer (decrypt + decode on queue, dequeue in order).
    buffer: Arc<Mutex<RaopBuffer>>,
    /// Shared mutable state for cross-task event delivery.
    state: Arc<Mutex<RtpState>>,
    /// Immediate gate that prevents the receive timer from consuming queued
    /// frames while a classic RTSP session is paused.
    paused: Arc<AtomicBool>,
    /// Monotonic pause start plus one; zero means active and `u64::MAX` means
    /// a resume adjustment is currently being applied.
    pause_started_ns: Arc<AtomicU64>,
    /// Send `true` to shut down the receive task.
    shutdown_tx: Option<watch::Sender<bool>>,
    /// Abort handle used by server-wide last-session-wins media control.
    receive_task: Option<tokio::task::AbortHandle>,
    /// iPhone's control port (0 = no retransmits).
    control_rport: u16,
    /// Local control port (bound by us).
    pub(crate) control_lport: u16,
    /// Local timing port (bound by us).
    pub(crate) timing_lport: u16,
    /// Local data port (bound by us).
    pub(crate) data_lport: u16,
    /// Full socket address of the remote peer.
    remote_socket: std::net::SocketAddr,
}

/// Build a resampler for the AP1 RTP path: `Some` when an explicit output rate
/// differs from the source rate, otherwise `None` (native-rate passthrough).
/// Shared by the UDP and TCP receive arms of [`RaopRtp::start`].
#[cfg(feature = "resample")]
fn make_resampler(
    output_sample_rate: Option<u32>,
    src_sample_rate: u32,
    channels: usize,
) -> Option<crate::codec::resample::StreamResampler> {
    match output_sample_rate {
        Some(target) if target != src_sample_rate => {
            crate::codec::resample::StreamResampler::new(src_sample_rate, target, channels)
        }
        _ => None,
    }
}

/// Present every queued frame whose RTP timestamp is due.
///
/// This is called after packet activity and from a short timer. The timer is
/// essential at the end of a track: classic AirPlay sends its final audio
/// packets ahead of their presentation time, so there may be no later data
/// packet to wake the receiver when those frames become due.
async fn drain_due_frames(
    buffer: &Arc<Mutex<RaopBuffer>>,
    state: &Arc<Mutex<RtpState>>,
    paused: &AtomicBool,
    no_resend: bool,
    sample_rate: u32,
    session: &mut dyn crate::raop::AudioSession,
    #[cfg(feature = "resample")] resampler: &mut Option<crate::codec::resample::StreamResampler>,
) {
    if paused.load(Ordering::Acquire) {
        return;
    }
    loop {
        if paused.load(Ordering::Acquire) {
            break;
        }
        let Some(timestamp) = buffer.lock().await.next_timestamp(no_resend) else {
            break;
        };
        let due = state
            .lock()
            .await
            .playout_clock
            .frame_is_due(timestamp, sample_rate, local_clock_ns());
        if !due {
            break;
        }
        let samples = {
            let mut queued = buffer.lock().await;
            queued
                .dequeue_with_timestamp(no_resend)
                .map(|(_, samples)| samples.to_vec())
        };
        let Some(samples) = samples else {
            break;
        };
        #[cfg(feature = "resample")]
        if let Some(resampler) = resampler {
            let resampled = resampler.process(&samples);
            session.audio_process(&resampled);
        } else {
            session.audio_process(&samples);
        }
        #[cfg(not(feature = "resample"))]
        session.audio_process(&samples);
    }
}

impl RaopRtp {
    /// Create a new RTP session from SDP parameters and AES session keys.
    /// Does not bind sockets or start receiving — call [`start`](Self::start) for that.
    ///
    /// Returns `None` if the (peer-supplied) `fmtp` attribute is malformed.
    pub(crate) fn new(callbacks: Arc<dyn AudioHandler>, config: RtpConfig) -> Option<Self> {
        let buffer = RaopBuffer::new(&config.rtpmap, &config.fmtp, &config.aes_key, &config.aes_iv)?;
        let alac_config = buffer.config().clone();
        Some(Self {
            handler: callbacks,
            remote: config.remote,
            local_addr: config.local_addr,
            output_sample_rate: config.output_sample_rate,
            remote_socket: config.remote_socket,
            config: alac_config,
            buffer: Arc::new(Mutex::new(buffer)),
            state: Arc::new(Mutex::new(RtpState {
                flush: NO_FLUSH,
                playout_clock: ClassicPlayoutClock::default(),
            })),
            paused: Arc::new(AtomicBool::new(false)),
            pause_started_ns: Arc::new(AtomicU64::new(0)),
            shutdown_tx: None,
            receive_task: None,
            control_rport: 0,
            control_lport: 0,
            timing_lport: 0,
            data_lport: 0,
        })
    }

    /// Bind UDP/TCP sockets and spawn the async receive task.
    ///
    /// Returns `(control_port, timing_port, data_port)` — the local ports
    /// that the iPhone should send RTP packets to.
    ///
    /// # Transport modes
    ///
    /// - `use_udp = true`: binds 3 UDP sockets (data, control, timing).
    ///   Control channel receives retransmit responses (RTP payload type 0x56).
    /// - `use_udp = false`: binds 1 TCP listener. iPhone connects and sends
    ///   `$`-prefixed interleaved RTP frames.
    pub(crate) fn start(
        &mut self,
        use_udp: bool,
        control_rport: u16,
        timing_rport: u16,
    ) -> Result<(u16, u16, u16), ShairplayError> {
        self.control_rport = control_rport;
        info!(use_udp, control_rport, timing_rport, remote = %self.remote, "AP1 RTP starting");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        if use_udp {
            let bind_addr = SocketAddr::new(rtp_bind_addr(self.local_addr), 0);
            let csock = bind_udp(bind_addr)?;
            let tsock = bind_udp(bind_addr)?;
            let dsock = bind_udp(bind_addr)?;
            self.control_lport = csock.local_addr().map_err(NetworkError::Io)?.port();
            self.timing_lport = tsock.local_addr().map_err(NetworkError::Io)?.port();
            self.data_lport = dsock.local_addr().map_err(NetworkError::Io)?.port();

            // Spawn NTP timing responder for this connection.
            let remote_sockaddr = self.remote_socket;
            let mut timing_addr = remote_sockaddr;
            timing_addr.set_port(timing_rport);
            super::ntp::spawn_ntp_responder(tsock, timing_addr);

            let config = self.config.clone();
            let mut session = self.handler.audio_init(AudioFormat {
                codec: AudioCodec::Pcm,
                bits: 32,
                channels: config.num_channels,
                sample_rate: self.output_sample_rate.unwrap_or(config.sample_rate),
                source: Some(SourceAudioFormat {
                    codec: SourceAudioCodec::Alac,
                    bits: Some(config.bit_depth),
                    channels: Some(config.num_channels),
                    sample_rate: Some(config.sample_rate),
                }),
            });

            #[cfg(feature = "resample")]
            let mut resampler = make_resampler(
                self.output_sample_rate,
                config.sample_rate,
                config.num_channels as usize,
            );

            let buffer = self.buffer.clone();
            let state = self.state.clone();
            let paused = self.paused.clone();
            // If control_rport is 0, the iPhone doesn't support retransmits.
            let no_resend = control_rport == 0;
            let mut remote_control_addr = self.remote_socket;
            remote_control_addr.set_port(control_rport);
            let _remote_for_task = self.remote.clone();

            let task = tokio::spawn(async move {
                let mut shutdown_rx = shutdown_rx;
                let mut data_packet = [0u8; RAOP_PACKET_LEN];
                let mut ctrl_packet = [0u8; RAOP_PACKET_LEN];
                let mut playout_tick = tokio::time::interval(Duration::from_millis(4));
                playout_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut next_resend_check = Instant::now() + RESEND_FIRST_CHECK_DELAY;
                loop {
                    // Drain flush events only — metadata goes through AudioHandler now.
                    {
                        let mut st = state.lock().await;
                        if st.flush != NO_FLUSH {
                            buffer.lock().await.flush(st.flush);
                            session.audio_flush();
                            st.flush = NO_FLUSH;
                        }
                    }

                    tokio::select! {
                        // Data channel: audio RTP packets.
                        result = dsock.recv_from(&mut data_packet) => {
                            if let Ok((len, _)) = result
                                && len >= 12 {
                                    let payload_type = data_packet[1] & 0x7f;
                                    let packet = if payload_type == CTRL_PAYLOAD_TYPE
                                        && len > RETRANSMIT_HEADER_LEN
                                    {
                                        &data_packet[RETRANSMIT_HEADER_LEN..len]
                                    } else {
                                        &data_packet[..len]
                                    };
                                    buffer.lock().await.queue(packet, true);
                                }
                        }
                        // Control channel: clock sync (0x54) and retransmit
                        // responses (0x56).
                        result = csock.recv_from(&mut ctrl_packet) => {
                            if let Ok((len, _)) = result && len >= 2 {
                                let payload_type = ctrl_packet[1] & 0x7f;
                                if payload_type == CTRL_SYNC_PAYLOAD_TYPE
                                    && !paused.load(Ordering::Acquire)
                                    && state
                                        .lock()
                                        .await
                                        .playout_clock
                                        .observe_sync(&ctrl_packet[..len], local_clock_ns())
                                {
                                    tracing::trace!("Classic RAOP playout clock synchronized");
                                } else if payload_type == CTRL_PAYLOAD_TYPE && len >= 12 {
                                    let mut buf = buffer.lock().await;
                                    // Retransmit packets have a 4-byte header before the original RTP.
                                    if len > RETRANSMIT_HEADER_LEN { buf.queue(&ctrl_packet[RETRANSMIT_HEADER_LEN..len], true); }
                                }
                            }
                        }
                        _ = playout_tick.tick() => {}
                        _ = shutdown_rx.changed() => break,
                    }
                    if !no_resend && Instant::now() >= next_resend_check {
                        let missing = buffer.lock().await.missing_runs(MAX_RESEND_RUNS_PER_CHECK);
                        for (first, count) in missing {
                            let request = resend_request(first, count);
                            if let Err(error) = csock.send_to(&request, remote_control_addr).await {
                                tracing::debug!(
                                    %error,
                                    first,
                                    count,
                                    "Unable to request AirPlay RTP retransmission"
                                );
                            }
                        }
                        next_resend_check = Instant::now() + RESEND_REPEAT_INTERVAL;
                    }
                    drain_due_frames(
                        &buffer,
                        &state,
                        paused.as_ref(),
                        no_resend,
                        config.sample_rate,
                        session.as_mut(),
                        #[cfg(feature = "resample")]
                        &mut resampler,
                    )
                    .await;
                }
                // AudioSession dropped here → triggers cleanup in the app.
            });
            self.receive_task = Some(task.abort_handle());
        } else {
            // TCP interleaved mode: single connection, `$`-prefixed framing.
            let listener = bind_tcp(SocketAddr::new(rtp_bind_addr(self.local_addr), 0))?;
            self.data_lport = listener.local_addr().map_err(NetworkError::Io)?.port();

            let config = self.config.clone();
            let mut session = self.handler.audio_init(AudioFormat {
                codec: AudioCodec::Pcm,
                bits: 32,
                channels: config.num_channels,
                sample_rate: self.output_sample_rate.unwrap_or(config.sample_rate),
                source: Some(SourceAudioFormat {
                    codec: SourceAudioCodec::Alac,
                    bits: Some(config.bit_depth),
                    channels: Some(config.num_channels),
                    sample_rate: Some(config.sample_rate),
                }),
            });

            #[cfg(feature = "resample")]
            let mut resampler = make_resampler(
                self.output_sample_rate,
                config.sample_rate,
                config.num_channels as usize,
            );

            let buffer = self.buffer.clone();
            let state = self.state.clone();
            let paused = self.paused.clone();
            let _remote_for_tcp = self.remote.clone();

            let task = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut shutdown_rx = shutdown_rx;

                // Wait for the iPhone to connect.
                let stream = tokio::select! {
                    result = listener.accept() => match result {
                        Ok((s, _)) => s,
                        Err(_) => return,
                    },
                    _ = shutdown_rx.changed() => return,
                };

                let mut reader = tokio::io::BufReader::new(stream);
                let mut packet_buf = Vec::with_capacity(RAOP_PACKET_LEN + 4);
                let mut read_buf = [0u8; 4096];
                let mut input_closed = false;
                let mut playout_tick = tokio::time::interval(Duration::from_millis(4));
                playout_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                'tcp: loop {
                    // Drain flush events only — metadata goes through AudioHandler now.
                    {
                        let mut st = state.lock().await;
                        if st.flush != NO_FLUSH {
                            buffer.lock().await.flush(st.flush);
                            session.audio_flush();
                            st.flush = NO_FLUSH;
                        }
                    }

                    tokio::select! {
                        result = reader.read(&mut read_buf), if !input_closed => {
                            match result {
                                Ok(0) | Err(_) => {
                                    input_closed = true;
                                    continue 'tcp;
                                }
                                Ok(n) => packet_buf.extend_from_slice(&read_buf[..n]),
                            }
                            if packet_buf.len() > RAOP_PACKET_LEN * 4 {
                                tracing::warn!("TCP RTP buffer exceeded safety limit");
                                break;
                            }
                            // TCP interleaved: each frame is `$ <channel> <len_hi> <len_lo> <rtp...>`.
                            while packet_buf.len() >= 4 {
                                if packet_buf[0] != b'$' {
                                    packet_buf.drain(..1);
                                    continue;
                                }
                                let channel = packet_buf[1];
                                let rtp_len = ((packet_buf[2] as usize) << 8) | packet_buf[3] as usize;
                                if rtp_len > RAOP_PACKET_LEN {
                                    tracing::warn!(rtp_len, "TCP RTP frame exceeded maximum size, closing");
                                    packet_buf.clear();
                                    break 'tcp;
                                }
                                if packet_buf.len() < 4 + rtp_len { break; }
                                let frame = packet_buf[4..4 + rtp_len].to_vec();
                                packet_buf.drain(..4 + rtp_len);

                                if channel != 0 {
                                    if frame.len() >= 2
                                        && frame[1] & 0x7f == CTRL_SYNC_PAYLOAD_TYPE
                                        && !paused.load(Ordering::Acquire)
                                        && state
                                            .lock()
                                            .await
                                            .playout_clock
                                            .observe_sync(&frame, local_clock_ns())
                                    {
                                        tracing::trace!(
                                            "Classic RAOP TCP playout clock synchronized"
                                        );
                                    }
                                    continue;
                                }

                                // RTP sequence numbers remain valid on the
                                // reliable interleaved transport. Retaining
                                // them is necessary now that frames wait in
                                // the buffer for their presentation time.
                                buffer.lock().await.queue(&frame, true);
                            }
                        }
                        _ = playout_tick.tick() => {}
                        _ = shutdown_rx.changed() => break,
                    }
                    drain_due_frames(
                        &buffer,
                        &state,
                        paused.as_ref(),
                        true,
                        config.sample_rate,
                        session.as_mut(),
                        #[cfg(feature = "resample")]
                        &mut resampler,
                    )
                    .await;
                    if input_closed
                        && (paused.load(Ordering::Acquire) || buffer.lock().await.next_timestamp(true).is_none())
                    {
                        break;
                    }
                }
            });
            self.receive_task = Some(task.abort_handle());
        }

        Ok((self.control_lport, self.timing_lport, self.data_lport))
    }

    /// Request a buffer flush up to the given sequence number.
    pub(crate) fn flush(&self, next_seq: i32) {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut state = state.lock().await;
            state.flush = next_seq;
            state.playout_clock.reset();
        });
    }

    /// Freeze classic RTP playout without discarding the sender's look-ahead.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        self.pause_started_ns
            .store(local_clock_ns().saturating_add(1), Ordering::Release);
    }

    /// Resume the same classic RTP timeline after an RTSP PAUSE.
    pub(crate) fn resume(&self) {
        let marker = self.pause_started_ns.load(Ordering::Acquire);
        if marker == 0 {
            self.paused.store(false, Ordering::Release);
            return;
        }
        if marker == u64::MAX
            || self
                .pause_started_ns
                .compare_exchange(marker, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let state = self.state.clone();
        let paused = self.paused.clone();
        let pause_started_ns = self.pause_started_ns.clone();
        tokio::spawn(async move {
            let paused_at = marker.saturating_sub(1);
            let paused_duration_ns = local_clock_ns().saturating_sub(paused_at);
            state
                .lock()
                .await
                .playout_clock
                .shift_local_timeline(paused_duration_ns);
            if pause_started_ns
                .compare_exchange(u64::MAX, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                paused.store(false, Ordering::Release);
            }
        });
    }

    pub(crate) fn stop_handle(&self) -> Option<tokio::task::AbortHandle> {
        self.receive_task.clone()
    }

    /// Stop the receive task and flush the buffer.
    pub(crate) fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        self.receive_task.take();
        self.flush(-1);
    }
}

#[cfg(test)]
mod tests {
    use super::{CTRL_SYNC_PAYLOAD_TYPE, ClassicPlayoutClock, INITIAL_SYNC_WAIT_NS, resend_request};

    fn sync_packet_with_ntp(current_rtp: u32, next_rtp: u32, ntp: u64) -> [u8; 20] {
        let mut packet = [0_u8; 20];
        packet[0] = 0x80;
        packet[1] = 0x80 | CTRL_SYNC_PAYLOAD_TYPE;
        packet[4..8].copy_from_slice(&current_rtp.to_be_bytes());
        packet[8..16].copy_from_slice(&ntp.to_be_bytes());
        packet[16..20].copy_from_slice(&next_rtp.to_be_bytes());
        packet
    }

    fn sync_packet(current_rtp: u32, next_rtp: u32) -> [u8; 20] {
        sync_packet_with_ntp(current_rtp, next_rtp, 0x83ab_1c49_0000_0000)
    }

    #[test]
    fn classic_sync_uses_current_playout_timestamp_not_future_packet() {
        let mut clock = ClassicPlayoutClock::default();
        let local_anchor = 10_000_000_000;
        assert!(clock.observe_sync(&sync_packet(44_100, 121_275), local_anchor,));

        assert!(!clock.frame_is_due(88_200, 44_100, local_anchor + 999_999_999,));
        assert!(clock.frame_is_due(88_200, 44_100, local_anchor + 1_000_000_000,));
    }

    #[test]
    fn classic_sync_rtp_delta_handles_u32_wraparound() {
        let mut clock = ClassicPlayoutClock::default();
        let local_anchor = 20_000_000_000;
        assert!(clock.observe_sync(&sync_packet(u32::MAX - 99, 50_000), local_anchor,));

        assert!(!clock.frame_is_due(100, 1_000, local_anchor + 199_999_999,));
        assert!(clock.frame_is_due(100, 1_000, local_anchor + 200_000_000,));
    }

    #[test]
    fn non_sync_sender_falls_back_after_bounded_wait() {
        let mut clock = ClassicPlayoutClock::default();
        let first_audio = 30_000_000_000;

        assert!(!clock.frame_is_due(1_000, 44_100, first_audio));
        assert!(!clock.frame_is_due(1_000, 44_100, first_audio + INITIAL_SYNC_WAIT_NS - 1,));
        assert!(clock.frame_is_due(1_000, 44_100, first_audio + INITIAL_SYNC_WAIT_NS,));
        assert!(
            !clock.frame_is_due(45_100, 44_100, first_audio + INITIAL_SYNC_WAIT_NS,),
            "fallback must preserve RTP pacing instead of dumping the queue"
        );
        assert!(clock.frame_is_due(45_100, 44_100, first_audio + INITIAL_SYNC_WAIT_NS + 1_000_000_000,));
    }

    #[test]
    fn later_sync_uses_ntp_delta_instead_of_receipt_jitter() {
        let mut clock = ClassicPlayoutClock::default();
        let first_local = 10_000_000_000;
        assert!(clock.observe_sync(&sync_packet_with_ntp(10_000, 20_000, 100_u64 << 32), first_local,));
        assert!(clock.observe_sync(
            &sync_packet_with_ntp(54_100, 64_100, 101_u64 << 32),
            first_local + 1_200_000_000,
        ));

        assert!(!clock.frame_is_due(54_100, 44_100, first_local + 999_999_999));
        assert!(clock.frame_is_due(54_100, 44_100, first_local + 1_000_000_000));
    }

    #[test]
    fn pause_duration_shifts_the_local_playout_timeline() {
        let mut clock = ClassicPlayoutClock::default();
        let first_local = 20_000_000_000;
        assert!(clock.observe_sync(&sync_packet(1_000, 2_000), first_local));
        clock.shift_local_timeline(2_000_000_000);

        assert!(!clock.frame_is_due(1_000, 44_100, first_local + 1_999_999_999));
        assert!(clock.frame_is_due(1_000, 44_100, first_local + 2_000_000_000));

        let resumed_local = first_local + 2_100_000_000;
        assert!(clock.observe_sync(
            &sync_packet_with_ntp(1_000, 2_000, 0x83ab_1c4b_0000_0000,),
            resumed_local,
        ));
        assert!(
            clock.frame_is_due(1_000, 44_100, resumed_local),
            "the first post-pause sync must re-anchor, not add pause time twice"
        );
    }

    #[test]
    fn resend_request_uses_the_airplay_sequence_range_wire_format() {
        assert_eq!(
            resend_request(0x1234, 3),
            [0x80, 0xd5, 0x00, 0x01, 0x12, 0x34, 0x00, 0x03]
        );
    }
}
