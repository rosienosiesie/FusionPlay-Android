//! RTSP request handlers for AP1 and AP2 AirPlay sessions.

use std::sync::Arc;

use crate::crypto::fairplay::FairPlay;
use crate::crypto::pairing::PairingSession;
use crate::error::{CodecError, ShairplayError};
use crate::proto::http::{HttpRequest, HttpResponse};
use crate::proto::sdp::Sdp;
use crate::raop::rtp::RaopRtp;

#[cfg(feature = "ap2")]
use crate::crypto::pairing_homekit::{PairVerifyServer, SrpServer};

/// Per-connection state for RTSP handler dispatch. Equivalent to raop_conn_t.
pub(crate) struct RaopConnection {
    pub(crate) raop_rtp: Option<RaopRtp>,
    pub(crate) fairplay: FairPlay,
    pub(crate) pairing: PairingSession,
    pub(crate) local_addr: Vec<u8>,
    #[allow(dead_code)] // read in AP2 event channel binding
    pub(crate) remote_addr: Vec<u8>,
    pub(crate) remote_socket: std::net::SocketAddr,
    pub(crate) nonce: String,
    /// Cancellation handle for the accepted RTSP connection that owns this
    /// request/session.
    pub(crate) close_handle: crate::net::server::ConnectionCloseHandle,
    /// Cheap shared handle to server-wide config (identity, keys, handler, settings).
    /// Replaces the ~17 fields that were previously deep-copied into every connection.
    pub(crate) shared: Arc<crate::raop::connection::RaopShared>,
    // AirPlay 2 state
    #[cfg(feature = "ap2")]
    pub(crate) srp_server: Option<SrpServer>,
    #[cfg(feature = "ap2")]
    pub(crate) pair_verify: Option<PairVerifyServer>,
    #[cfg(feature = "ap2")]
    pub(crate) ap2_shared_secret: Option<Vec<u8>>,
    /// X25519 shared secret from pair-verify (32 bytes). Used for video key derivation.
    #[cfg(feature = "ap2")]
    pub(crate) pair_verify_secret: Option<[u8; 32]>,
    #[cfg(feature = "ap2")]
    pub(crate) is_ap2: bool,
    #[cfg(feature = "ap2")]
    pub(crate) playout_cmd: Option<tokio::sync::mpsc::UnboundedSender<crate::raop::buffered_audio::PlayoutCommand>>,
    #[cfg(feature = "ap2")]
    pub(crate) event_sender: Option<crate::raop::event_channel::EventSender>,
    #[cfg(feature = "ap2")]
    pub(crate) ap2_remote_control: Option<std::sync::Arc<crate::raop::Ap2RemoteControl>>,
    #[cfg(feature = "ap2")]
    pub(crate) dacp_id: Option<String>,
    #[cfg(feature = "ap2")]
    pub(crate) active_remote: Option<String>,
    #[cfg(feature = "ap2")]
    pub(crate) published_dacp_credentials: Option<(String, String)>,
    #[cfg(feature = "video")]
    pub(crate) ekey: Option<[u8; 16]>,
    #[cfg(feature = "video")]
    pub(crate) eiv: Option<[u8; 16]>,
    #[cfg(feature = "hls")]
    pub(crate) hls_state: std::sync::Arc<std::sync::Mutex<crate::raop::hls::HlsState>>,
}

/// Returns the connection's local IP address.
pub(crate) fn local_ip_from(conn: &RaopConnection) -> std::net::IpAddr {
    ip_from_bytes(&conn.local_addr)
}

pub(crate) fn ip_from_bytes(bytes: &[u8]) -> std::net::IpAddr {
    if bytes.len() == 16 {
        let ip: [u8; 16] = bytes[..16].try_into().unwrap_or([0; 16]);
        std::net::IpAddr::V6(std::net::Ipv6Addr::from(ip))
    } else {
        let ip: [u8; 4] = bytes[..4].try_into().unwrap_or([0; 4]);
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(ip))
    }
}

/// Collect DACP credentials from every authenticated RTSP request.
///
/// Classic senders are not consistent about which request carries DACP-ID and
/// Active-Remote, and the two headers may arrive separately. Keeping them on
/// the connection lets controls remain available across PAUSE/RECORD and
/// publishes a new remote only when the credential pair actually changes.
#[cfg(feature = "ap2")]
pub(crate) fn capture_dacp_remote_control(conn: &mut RaopConnection, request: &HttpRequest) {
    if let Some(value) = request.header("DACP-ID").filter(|value| !value.is_empty()) {
        conn.dacp_id = Some(value.to_owned());
    }
    if let Some(value) = request.header("Active-Remote").filter(|value| !value.is_empty()) {
        conn.active_remote = Some(value.to_owned());
    }
    let (Some(dacp_id), Some(active_remote)) = (&conn.dacp_id, &conn.active_remote) else {
        return;
    };
    let credentials = (dacp_id.clone(), active_remote.clone());
    if conn.published_dacp_credentials.as_ref() == Some(&credentials) {
        return;
    }

    let addr_bytes = crate::raop::rtp::remote_addr_bytes(&conn.remote_socket.ip().to_string());
    let remote = std::sync::Arc::new(crate::raop::DacpRemoteControl::new(dacp_id, active_remote, &addr_bytes));
    conn.published_dacp_credentials = Some(credentials);
    conn.shared.handler.on_remote_control(remote);
}

/// Returns a bind address for sub-listeners (buffered audio, event channel, etc.).
/// Uses the specific local IP for routable addresses (respects BindConfig).
/// Falls back to unspecified for link-local IPv6.
#[cfg(feature = "ap2")]
pub(crate) fn bind_addr_for(conn: &RaopConnection) -> std::net::SocketAddr {
    let ip = local_ip_from(conn);
    let bind_ip = match ip {
        std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80 => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        }
        other => other,
    };
    std::net::SocketAddr::new(bind_ip, 0)
}
/// AP1 pair-setup: return Ed25519 public key.
pub(crate) fn handle_pair_setup(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let data = request.data()?;
    if data.len() != 32 {
        return None;
    }
    let public_key = conn.shared.pairing.public_key();
    response.add_header("Content-Type", "application/octet-stream");
    Some(public_key.to_vec())
}

/// AP1 pair-verify: Ed25519/Curve25519 handshake (M1/M2).
pub(crate) fn handle_pair_verify(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let data = request.data()?;
    if data.len() < 4 {
        return None;
    }

    match data[0] {
        1 => {
            if data.len() != 4 + 32 + 32 {
                return None;
            }
            let ecdh_key: &[u8; 32] = data[4..36].try_into().ok()?;
            let ed_key: &[u8; 32] = data[36..68].try_into().ok()?;
            let _ = conn.pairing.handshake(ecdh_key, ed_key);
            let public_key = conn.pairing.get_public_key().ok()?;
            let signature = conn.pairing.get_signature().ok()?;
            response.add_header("Content-Type", "application/octet-stream");
            let mut resp = Vec::with_capacity(96);
            resp.extend_from_slice(&public_key);
            resp.extend_from_slice(&signature);
            Some(resp)
        }
        0 => {
            if data.len() != 4 + 64 {
                return None;
            }
            let sig: &[u8; 64] = data[4..68].try_into().ok()?;
            if let Err(e) = conn.pairing.finish(sig) {
                tracing::warn!("AP1 pair-verify finish failed: {e}");
                conn.shared.handler.on_error(&ShairplayError::Crypto(e));
                response.set_disconnect(true);
            }
            None
        }
        _ => None,
    }
}

/// FairPlay DRM handshake (fp-setup M1/M2).
pub(crate) fn handle_fp_setup(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let data = request.data()?;
    tracing::debug!(data_len = data.len(), "fp-setup");
    match data.len() {
        16 => {
            let req: &[u8; 16] = data.try_into().ok()?;
            let res = match conn.fairplay.setup(req) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("fp-setup M1 failed: {e}");
                    conn.shared.handler.on_error(&ShairplayError::Crypto(e));
                    return None;
                }
            };
            Some(res.to_vec())
        }
        164 => {
            let req: &[u8; 164] = data.try_into().ok()?;
            let res = match conn.fairplay.handshake(req) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("fp-setup M2 failed: {e}");
                    conn.shared.handler.on_error(&ShairplayError::Crypto(e));
                    return None;
                }
            };
            Some(res.to_vec())
        }
        _ => None,
    }
}

/// RTSP OPTIONS: return supported methods.
pub(crate) fn handle_options(
    _conn: &mut RaopConnection,
    _request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    #[cfg(feature = "ap2")]
    response.add_header(
        "Public",
        "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, FLUSHBUFFERED, TEARDOWN, OPTIONS, POST, GET, PUT",
    );
    #[cfg(not(feature = "ap2"))]
    response.add_header(
        "Public",
        "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
    );
    None
}

/// RTSP ANNOUNCE: parse SDP, extract AES keys, create RTP session.
pub(crate) fn handle_announce(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let data = request.data()?;
    let sdp_str = std::str::from_utf8(data).ok()?;
    let sdp = Sdp::parse(sdp_str);

    let remote = sdp.connection()?;
    let rtpmap = sdp.rtpmap()?;
    let fmtp = sdp.fmtp()?;
    let aesiv_str = sdp.aesiv()?;

    let mut aeskey = [0u8; 16];
    let mut aesiv = [0u8; 16];

    // Decrypt AES key from RSA or FairPlay
    let key_bytes = if let Some(rsa_key_str) = sdp.rsaaeskey() {
        match conn.shared.rsakey.decrypt(rsa_key_str) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!("ANNOUNCE rsaaeskey decrypt failed: {e}");
                conn.shared.handler.on_error(&ShairplayError::Crypto(e));
                None
            }
        }
    } else if let Some(fp_key_str) = sdp.fpaeskey() {
        let fp_data = conn.shared.rsakey.decode(fp_key_str).ok()?;
        if fp_data.len() == 72 {
            let input: &[u8; 72] = fp_data.as_slice().try_into().ok()?;
            match conn.fairplay.decrypt(input) {
                Ok(key) => Some(key.to_vec()),
                Err(e) => {
                    tracing::warn!("ANNOUNCE fpaeskey decrypt failed: {e}");
                    conn.shared.handler.on_error(&ShairplayError::Crypto(e));
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let key_bytes = key_bytes?;
    if key_bytes.len() >= 16 {
        aeskey.copy_from_slice(&key_bytes[..16]);
    }

    let iv_bytes = conn.shared.rsakey.decode(aesiv_str).ok()?;
    if iv_bytes.len() >= 16 {
        aesiv.copy_from_slice(&iv_bytes[..16]);
    }

    // Destroy existing RTP session if any
    conn.raop_rtp = None;

    conn.raop_rtp = RaopRtp::new(
        conn.shared.handler.clone(),
        crate::raop::rtp::RtpConfig {
            remote: remote.to_string(),
            local_addr: local_ip_from(conn),
            rtpmap: rtpmap.to_string(),
            fmtp: fmtp.to_string(),
            aes_key: aeskey,
            aes_iv: aesiv,
            output_sample_rate: conn.shared.output_sample_rate,
            remote_socket: conn.remote_socket,
        },
    );

    if conn.raop_rtp.is_none() {
        tracing::warn!(rtpmap, fmtp, "ANNOUNCE: RaopRtp::new failed (malformed fmtp)");
        conn.shared
            .handler
            .on_error(&ShairplayError::Codec(CodecError::UnsupportedFormat(format!(
                "ANNOUNCE rtpmap={rtpmap} fmtp={fmtp}"
            ))));
        response.set_disconnect(true);
    }
    None
}

/// AP1 RTSP SETUP: bind RTP ports and start audio receiver.
pub(crate) fn handle_setup(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let transport = request.header("Transport")?;
    tracing::debug!(transport, "AP1 SETUP");

    // Without the AP2 feature there is no per-connection credential cache, so
    // preserve the classic same-request fallback.
    #[cfg(not(feature = "ap2"))]
    if let (Some(dacp_id), Some(active_remote)) = (request.header("DACP-ID"), request.header("Active-Remote")) {
        let addr_bytes = crate::raop::rtp::remote_addr_bytes(&conn.remote_socket.ip().to_string());
        let remote = std::sync::Arc::new(crate::raop::DacpRemoteControl::new(dacp_id, active_remote, &addr_bytes));
        conn.shared.handler.on_remote_control(remote);
    }

    let use_udp = !transport.starts_with("RTP/AVP/TCP");
    let mut remote_cport = 0u16;
    let mut remote_tport = 0u16;

    if use_udp {
        for part in transport.split(';') {
            if let Some(val) = part.strip_prefix("control_port=") {
                remote_cport = val.parse().unwrap_or(0);
            } else if let Some(val) = part.strip_prefix("timing_port=") {
                remote_tport = val.parse().unwrap_or(0);
            }
        }
    }

    if let Some(rtp) = &mut conn.raop_rtp {
        let (cport, tport, dport) = match rtp.start(use_udp, remote_cport, remote_tport) {
            Ok(ports) => ports,
            Err(e) => {
                tracing::warn!("AP1 SETUP rtp.start failed: {e}");
                conn.shared.handler.on_error(&e);
                return None;
            }
        };
        if let Some(stop_handle) = rtp.stop_handle() {
            conn.shared
                .set_active_audio(conn.close_handle.clone(), Box::new(move || stop_handle.abort()));
        }

        let transport_resp = if use_udp {
            format!(
                "RTP/AVP/UDP;unicast;mode=record;timing_port={tport};events;control_port={cport};server_port={dport}"
            )
        } else {
            format!("RTP/AVP/TCP;unicast;interleaved=0-1;mode=record;server_port={dport}")
        };
        response.add_header("Transport", &transport_resp);
        response.add_header("Session", "DEADBEEF");
    } else {
        response.set_disconnect(true);
    }
    None
}

/// AP1 RTSP RECORD: acknowledge the start of RTP streaming.
pub(crate) fn handle_record(
    conn: &mut RaopConnection,
    _request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    // Classic AirPlay interprets this as the receiver's minimum output
    // latency. 11,025 frames equals 250 ms at its 44.1 kHz clock and is the
    // value used by mature synchronized RAOP receivers.
    response.add_header("Audio-Latency", "11025");
    if let Some(rtp) = conn.raop_rtp.as_ref() {
        rtp.resume();
    }
    conn.shared.handler.on_playback_state(true);
    None
}

/// RTSP PAUSE: acknowledge the pause without tearing down the RTP stream.
///
/// Keeping the session alive is important because senders resume the same
/// session with RECORD (AP1) or SETRATEANCHORTIME (AP2).
pub(crate) fn handle_pause(
    conn: &mut RaopConnection,
    _request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    tracing::debug!("PAUSE");
    #[cfg(feature = "ap2")]
    let should_apply = !conn.is_ap2
        || conn
            .ap2_remote_control
            .as_ref()
            .is_none_or(|remote| remote.update_playback_rate(0));
    #[cfg(not(feature = "ap2"))]
    let should_apply = true;
    #[cfg(feature = "ap2")]
    if conn.is_ap2
        && should_apply
        && let Some(command_sender) = conn.playout_cmd.as_ref()
    {
        let _ = command_sender.send(crate::raop::buffered_audio::PlayoutCommand::Pause);
    }
    #[cfg(feature = "ap2")]
    let is_classic = !conn.is_ap2;
    #[cfg(not(feature = "ap2"))]
    let is_classic = true;
    if should_apply
        && is_classic
        && let Some(rtp) = conn.raop_rtp.as_ref()
    {
        rtp.pause();
    }
    if should_apply {
        conn.shared.handler.on_playback_state(false);
    }
    None
}

/// RTSP GET_PARAMETER: return volume or other parameters.
pub(crate) fn handle_get_parameter(
    _conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let content_type = request.header("Content-Type")?;
    if content_type != "text/parameters" {
        return None;
    }

    let data = request.data()?;
    let text = std::str::from_utf8(data).ok()?;
    if text.contains("volume") {
        response.add_header("Content-Type", "text/parameters");
        return Some(b"volume: 0.000000\r\n".to_vec());
    }
    None
}

/// RTSP SET_PARAMETER: handle volume, metadata, artwork, progress.
pub(crate) fn handle_set_parameter(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let content_type = request.header("Content-Type")?;
    let data = request.data()?;
    tracing::debug!(content_type, len = data.len(), "SET_PARAMETER");

    // Volume, progress, cover art, and DMAP metadata are delivered straight to the
    // AudioHandler (never blocking the audio pipeline). This dispatch is identical
    // for AP1 and AP2 — only audio-pipeline commands (rate/flush) differ, and those
    // arrive on their own RTSP methods, not here.
    match content_type {
        "text/parameters" => {
            let text = std::str::from_utf8(data).ok()?;
            if let Some(rest) = text.strip_prefix("volume: ") {
                if let Ok(vol) = rest.trim().parse::<f32>() {
                    conn.shared.handler.on_volume(vol);
                }
            } else if let Some(rest) = text.strip_prefix("progress: ") {
                let parts: Vec<&str> = rest.trim().split('/').collect();
                if parts.len() == 3 {
                    conn.shared.handler.on_progress(
                        parts[0].parse().unwrap_or(0),
                        parts[1].parse().unwrap_or(0),
                        parts[2].parse().unwrap_or(0),
                    );
                }
            }
        }
        "image/jpeg" | "image/png" => conn.shared.handler.on_coverart(data),
        "application/x-dmap-tagged" => {
            let meta = crate::proto::dmap::TrackMetadata::from_dmap(data);
            conn.shared.handler.on_metadata(&meta);
        }
        _ => {}
    }
    None
}

// --- AirPlay 2 handlers ---

// Verifies handler-facing notifications and RTSP transport state transitions.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::pairing::Pairing;
    use crate::crypto::rsa::RsaKey;
    #[cfg(feature = "ap2")]
    use crate::raop::MemoryPairingStore;
    use crate::raop::connection::RaopShared;
    use crate::raop::{AudioFormat, AudioHandler, AudioSession};
    use std::sync::{Arc, Mutex};

    /// An `AudioHandler` that records errors and transport state changes.
    #[derive(Default)]
    struct RecordingHandler {
        errors: Mutex<Vec<String>>,
        playback_states: Mutex<Vec<bool>>,
    }

    impl AudioHandler for RecordingHandler {
        fn audio_init(&self, _format: AudioFormat) -> Box<dyn AudioSession> {
            unreachable!("audio_init is not exercised by the fp-setup error path")
        }
        fn on_error(&self, error: &ShairplayError) {
            self.errors.lock().unwrap().push(error.to_string());
        }
        fn on_playback_state(&self, playing: bool) {
            self.playback_states.lock().unwrap().push(playing);
        }
    }

    fn test_connection(handler: Arc<RecordingHandler>) -> RaopConnection {
        let shared = Arc::new(RaopShared {
            rsakey: Arc::new(RsaKey::from_pem(include_str!("../../airport.key")).unwrap()),
            pairing: Arc::new(Pairing::generate().unwrap()),
            hwaddr: vec![0u8; 6],
            password: String::new(),
            handler,
            #[cfg(feature = "ap2")]
            pairing_store: Arc::new(MemoryPairingStore::default()),
            #[cfg(feature = "ap2")]
            identity_seed: [0u8; 32],
            output_sample_rate: None,
            output_max_channels: None,
            #[cfg(feature = "ap2")]
            pin: None,
            #[cfg(feature = "video")]
            video_handler: None,
            #[cfg(feature = "video")]
            video_ekey: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(feature = "video")]
            video_eiv: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(feature = "ap2")]
            pairing_id: "test-pairing".into(),
            #[cfg(feature = "ap2")]
            device_id: "00:00:00:00:00:00".into(),
            #[cfg(feature = "ap2")]
            airplay_name: "Windows".into(),
            #[cfg(feature = "ap2")]
            ptp_clock: crate::net::ptp::PtpClock::new(),
            active_audio: std::sync::Mutex::new(None),
            connections: std::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "hls")]
            hls_handler: None,
            #[cfg(feature = "hls")]
            hls_state: crate::raop::hls::HlsState::new(),
        });
        let (close_handle, _close_rx) =
            crate::net::server::ConnectionCloseHandle::new("127.0.0.1:5000".parse().unwrap());
        let pairing = shared.pairing.create_session();
        RaopConnection {
            raop_rtp: None,
            fairplay: FairPlay::new(),
            pairing,
            local_addr: vec![127, 0, 0, 1],
            remote_addr: vec![127, 0, 0, 1],
            remote_socket: "127.0.0.1:5000".parse().unwrap(),
            nonce: String::new(),
            close_handle,
            #[cfg(feature = "ap2")]
            srp_server: None,
            #[cfg(feature = "ap2")]
            pair_verify: None,
            #[cfg(feature = "ap2")]
            ap2_shared_secret: None,
            #[cfg(feature = "ap2")]
            pair_verify_secret: None,
            #[cfg(feature = "ap2")]
            is_ap2: false,
            #[cfg(feature = "ap2")]
            playout_cmd: None,
            #[cfg(feature = "ap2")]
            event_sender: None,
            #[cfg(feature = "ap2")]
            ap2_remote_control: None,
            #[cfg(feature = "ap2")]
            dacp_id: None,
            #[cfg(feature = "ap2")]
            active_remote: None,
            #[cfg(feature = "ap2")]
            published_dacp_credentials: None,
            #[cfg(feature = "video")]
            ekey: None,
            #[cfg(feature = "video")]
            eiv: None,
            #[cfg(feature = "hls")]
            hls_state: Arc::clone(&shared.hls_state),
            shared,
        }
    }

    fn fp_setup_request(body: &[u8]) -> HttpRequest {
        let mut msg = format!(
            "POST /fp-setup RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        msg.extend_from_slice(body);
        let mut req = HttpRequest::new();
        req.add_data(&msg).unwrap();
        assert_eq!(req.data().map(|d| d.len()), Some(body.len()), "body should parse");
        req
    }

    /// A malformed fp-setup M1 (version byte != 0x03) must surface the FairPlay
    /// `CryptoError` to the application via `on_error`, exactly once.
    #[test]
    fn fp_setup_failure_notifies_handler() {
        let handler = Arc::new(RecordingHandler::default());
        let mut conn = test_connection(handler.clone());

        // 16-byte M1 with byte[4] = 0x00 (!= 0x03) → FairPlay::setup returns Err.
        let req = fp_setup_request(&[0u8; 16]);
        let mut resp = HttpResponse::new("RTSP/1.0", 200, "OK");

        let out = handle_fp_setup(&mut conn, &req, &mut resp);

        assert!(out.is_none(), "malformed fp-setup should decline");
        let errors = handler.errors.lock().unwrap();
        assert_eq!(errors.len(), 1, "on_error should fire exactly once: {errors:?}");
        assert!(
            errors[0].contains("unsupported version"),
            "on_error should carry the FairPlay error, got: {:?}",
            errors[0]
        );
    }

    /// A well-formed fp-setup M1 must NOT trigger `on_error`.
    #[test]
    fn fp_setup_success_does_not_notify() {
        let handler = Arc::new(RecordingHandler::default());
        let mut conn = test_connection(handler.clone());

        // Valid M1: byte[4] = 0x03 (version), byte[14] = 0 (mode).
        let mut body = [0u8; 16];
        body[4] = 0x03;
        let req = fp_setup_request(&body);
        let mut resp = HttpResponse::new("RTSP/1.0", 200, "OK");

        let out = handle_fp_setup(&mut conn, &req, &mut resp);

        assert!(out.is_some(), "valid fp-setup M1 should produce a reply");
        assert!(
            handler.errors.lock().unwrap().is_empty(),
            "no error expected on success"
        );
    }

    fn request(method: &str) -> HttpRequest {
        let mut request = HttpRequest::new();
        request
            .add_data(format!("{method} rtsp://receiver/stream RTSP/1.0\r\nCSeq: 7\r\n\r\n").as_bytes())
            .unwrap();
        request
    }

    #[test]
    fn pause_then_record_keeps_connection_and_reports_transport_state() {
        let handler = Arc::new(RecordingHandler::default());
        let mut conn = test_connection(handler.clone());

        let pause_response = crate::raop::rtsp::dispatch(&mut conn, &request("PAUSE"));
        assert_eq!(pause_response.status_code(), 200);
        assert!(!pause_response.get_disconnect());
        assert!(
            conn.raop_rtp.is_none(),
            "PAUSE must not replace or tear down the RTP session"
        );

        let record_response = crate::raop::rtsp::dispatch(&mut conn, &request("RECORD"));
        assert_eq!(record_response.status_code(), 200);
        assert!(!record_response.get_disconnect());
        assert!(
            record_response
                .get_data()
                .windows(b"Audio-Latency: 11025\r\n".len())
                .any(|window| window == b"Audio-Latency: 11025\r\n"),
            "classic RECORD must advertise its 250 ms minimum latency"
        );
        assert_eq!(
            *handler.playback_states.lock().unwrap(),
            vec![false, true],
            "PAUSE and RECORD should update state without ending the session"
        );
    }

    #[cfg(feature = "ap2")]
    #[test]
    fn ap2_pause_stops_buffered_playout_without_stopping_the_session() {
        let handler = Arc::new(RecordingHandler::default());
        let mut conn = test_connection(handler.clone());
        conn.is_ap2 = true;
        let (command_sender, mut command_receiver) = tokio::sync::mpsc::unbounded_channel();
        conn.playout_cmd = Some(command_sender);

        let response = crate::raop::rtsp::dispatch(&mut conn, &request("PAUSE"));

        assert_eq!(response.status_code(), 200);
        assert!(!response.get_disconnect());
        assert!(
            conn.playout_cmd.is_some(),
            "PAUSE must preserve the playout command channel"
        );
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(crate::raop::buffered_audio::PlayoutCommand::Pause)
        ));

        let record_response = crate::raop::rtsp::dispatch(&mut conn, &request("RECORD"));
        assert_eq!(record_response.status_code(), 200);
        assert!(!record_response.get_disconnect());
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(crate::raop::buffered_audio::PlayoutCommand::Resume)
        ));
        assert_eq!(*handler.playback_states.lock().unwrap(), vec![false, true]);
    }
}
