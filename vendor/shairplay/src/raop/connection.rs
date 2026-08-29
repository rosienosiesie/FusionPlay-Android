//! Per-connection state and RTSP request handling.

use super::MAX_NONCE_LEN;
use super::handlers_ap1 as handlers;
use super::rtsp;
use super::types::*;
use crate::crypto::fairplay::FairPlay;
use crate::crypto::pairing::Pairing;
use crate::crypto::rsa::RsaKey;
use crate::net::server::ConnectionCloseHandle;
use crate::net::server::{ConnectionHandler, HttpdCallbacks};
use crate::proto::digest;
use crate::proto::http::{HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub(crate) struct ActiveAudio {
    stop: Box<dyn FnOnce() + Send>,
    owner: ConnectionCloseHandle,
}

/// Shared state passed to each connection.
pub(crate) struct RaopShared {
    pub(crate) rsakey: Arc<RsaKey>,
    pub(crate) pairing: Arc<Pairing>,
    pub(crate) hwaddr: Vec<u8>,
    pub(crate) password: String,
    pub(crate) handler: Arc<dyn AudioHandler>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_store: Arc<dyn PairingStore>,
    /// Accessory's long-term Ed25519 identity seed (random, persisted via the store).
    #[cfg(feature = "ap2")]
    pub(crate) identity_seed: [u8; 32],
    pub(crate) output_sample_rate: Option<u32>,
    /// Only consulted by the AP2 mixdown path; dead in AP1-only builds.
    #[cfg_attr(not(feature = "ap2"), allow(dead_code))]
    pub(crate) output_max_channels: Option<u8>,
    #[cfg(feature = "ap2")]
    pub(crate) pin: Option<String>,
    #[cfg(feature = "video")]
    pub(crate) video_handler: Option<Arc<dyn crate::raop::video::VideoHandler>>,
    /// Shared video encryption keys — set by audio SETUP, read by video SETUP.
    #[cfg(feature = "video")]
    pub(crate) video_ekey: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "video")]
    pub(crate) video_eiv: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_id: String,
    /// Accessory device id (`hwaddr_airplay(hwaddr)`), computed once at build.
    #[cfg(feature = "ap2")]
    pub(crate) device_id: String,
    #[cfg(feature = "ap2")]
    pub(crate) airplay_name: String,
    /// Server-wide mapping from the active PTP master clock to local time.
    #[cfg(feature = "ap2")]
    pub(crate) ptp_clock: crate::net::ptp::PtpClock,
    /// Stop-handle for the currently-active audio session. iOS opens parallel
    /// connections (Happy Eyeballs) and switches between them; registering each
    /// new session here — and stopping the previous — keeps only the newest
    /// playout feeding the output (avoids overlapping / post-disconnect audio).
    pub(crate) active_audio: std::sync::Mutex<Option<ActiveAudio>>,
    /// All currently accepted RTSP/HTTP connections. Active media owners use
    /// this registry to close every auxiliary connection from the old sender
    /// (control, event, feedback) during a cross-protocol takeover.
    pub(crate) connections: std::sync::Mutex<HashMap<u64, ConnectionCloseHandle>>,
    #[cfg(feature = "hls")]
    pub(crate) hls_handler: Option<Arc<dyn crate::raop::hls::HlsHandler>>,
    /// AirPlay video control requests are frequently sent over separate HTTP
    /// connections. Keep their session state server-wide so `/rate`, `/scrub`,
    /// `/playback-info`, and `/stop` can control the `/play` session regardless
    /// of which TCP connection carries the request.
    #[cfg(feature = "hls")]
    pub(crate) hls_state: Arc<std::sync::Mutex<crate::raop::hls::HlsState>>,
}

impl RaopShared {
    /// Register a newly-started audio session, stopping the previous one so only
    /// the latest connection's playout feeds the audio output.
    pub(crate) fn set_active_audio(&self, owner: ConnectionCloseHandle, stop: Box<dyn FnOnce() + Send>) {
        let prev = self.active_audio.lock().ok().and_then(|mut active| {
            active.replace(ActiveAudio {
                stop,
                owner: owner.clone(),
            })
        });
        tracing::info!(
            connection_id = owner.id(),
            previous_connection_id = prev.as_ref().map(|active| active.owner.id()),
            "Active AirPlay audio owner updated"
        );
        if let Some(prev) = prev {
            (prev.stop)();
            // A new AirPlay sender can replace an older one without passing
            // through the application arbiter. If it is a different sender,
            // close its auxiliary event/feedback/control connections as well
            // so the old device no longer believes it owns this receiver.
            // Multiple connections from the same IP can belong to one sender,
            // so only close the previous media owner in that case.
            if prev.owner.id() != owner.id() {
                if prev.owner.remote().ip() != owner.remote().ip() {
                    self.disconnect_sender(prev.owner.remote().ip());
                } else {
                    prev.owner.close();
                }
            }
        }
    }

    pub(crate) fn stop_active_audio(&self, disconnect_sender: bool) {
        let active = self.active_audio.lock().ok().and_then(|mut active| active.take());
        if let Some(active) = active {
            let sender_ip = active.owner.remote().ip();
            (active.stop)();
            if disconnect_sender {
                self.disconnect_sender(sender_ip);
            }
        }
    }

    /// Stop the active audio session only when it belongs to `owner_id`.
    ///
    /// AirPlay 2 uses several parallel RTSP connections for audio, events, and
    /// MediaRemote.  A TEARDOWN on an auxiliary connection must never stop the
    /// audio processor owned by another connection from the same sender.
    pub(crate) fn stop_active_audio_owned_by(&self, owner_id: u64) -> bool {
        let (active, current_owner_id) = self
            .active_audio
            .lock()
            .map(|mut active| {
                let current_owner_id = active.as_ref().map(|active| active.owner.id());
                let owned = if current_owner_id == Some(owner_id) {
                    active.take()
                } else {
                    None
                };
                (owned, current_owner_id)
            })
            .unwrap_or((None, None));
        if let Some(active) = active {
            tracing::info!(connection_id = owner_id, "Stopped owned AirPlay audio session");
            (active.stop)();
            true
        } else {
            tracing::info!(
                connection_id = owner_id,
                active_connection_id = current_owner_id,
                "Ignored audio stop from a non-owning AirPlay connection"
            );
            false
        }
    }

    pub(crate) fn register_connection(&self, handle: ConnectionCloseHandle) {
        if let Ok(mut connections) = self.connections.lock() {
            connections.insert(handle.id(), handle);
        }
    }

    pub(crate) fn unregister_connection(&self, id: u64) {
        if let Ok(mut connections) = self.connections.lock() {
            connections.remove(&id);
        }
    }

    pub(crate) fn disconnect_sender(&self, sender: std::net::IpAddr) {
        let connections = self
            .connections
            .lock()
            .map(|connections| {
                connections
                    .values()
                    .filter(|handle| handle.remote().ip() == sender)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for connection in connections {
            connection.close();
        }
    }

    #[cfg(feature = "hls")]
    pub(crate) fn stop_active_hls(&self, disconnect_sender: bool) {
        let active = self.hls_state.lock().ok().map(|mut state| {
            state.session_id = None;
            (state.session.take(), state.owner.take())
        });
        let Some((session, owner)) = active else {
            return;
        };
        if let Some(mut session) = session {
            session.stop();
        }
        if disconnect_sender && let Some(owner) = owner {
            self.disconnect_sender(owner.remote().ip());
        }
    }
}

impl HttpdCallbacks for RaopShared {
    fn conn_init(
        self: Arc<Self>,
        local: SocketAddr,
        remote: SocketAddr,
        close_handle: ConnectionCloseHandle,
    ) -> Option<Box<dyn ConnectionHandler>> {
        let local_bytes = match local.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        let remote_bytes = match remote.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };

        self.register_connection(close_handle.clone());
        let conn = handlers::RaopConnection {
            raop_rtp: None,
            fairplay: FairPlay::new(),
            pairing: self.pairing.create_session(),
            local_addr: local_bytes,
            remote_addr: remote_bytes,
            remote_socket: remote,
            nonce: digest::generate_nonce(MAX_NONCE_LEN),
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
            hls_state: Arc::clone(&self.hls_state),
            close_handle,
            shared: self.clone(),
        };
        let remote_str = remote.ip().to_string();
        conn.shared.handler.on_client_connected(&remote_str);
        Some(Box::new(RaopConnectionHandler {
            conn,
            remote_addr: remote_str,
            connected_at: std::time::Instant::now(),
            #[cfg(feature = "ap2")]
            cipher: None,
            #[cfg(feature = "ap2")]
            pending_secret: None,
        }))
    }
}

struct RaopConnectionHandler {
    conn: handlers::RaopConnection,
    remote_addr: String,
    /// Connection-start instant, used to log per-request elapsed time for
    /// connect-latency diagnostics (AP2 PTP-sync wait vs AP1 fast path).
    connected_at: std::time::Instant,
    #[cfg(feature = "ap2")]
    cipher: Option<crate::crypto::chacha_transport::EncryptedChannel>,
    #[cfg(feature = "ap2")]
    pending_secret: Option<Vec<u8>>,
}

impl Drop for RaopConnectionHandler {
    fn drop(&mut self) {
        self.conn.shared.unregister_connection(self.conn.close_handle.id());
        self.conn.shared.handler.on_client_disconnected(&self.remote_addr);
    }
}

impl ConnectionHandler for RaopConnectionHandler {
    fn conn_request(&mut self, request: &HttpRequest) -> HttpResponse {
        // Connect-latency timeline: one line per RTSP request, elapsed since the
        // connection opened. `/feedback` is a ~2s keep-alive heartbeat, so it drops
        // to `debug` to keep the connect sequence readable; everything else at info.
        let elapsed_ms = self.connected_at.elapsed().as_millis() as u64;
        let method = request.method().unwrap_or("");
        let url = request.url().unwrap_or("");
        let connection_id = self.conn.close_handle.id();
        if url == "/feedback" {
            tracing::debug!(connection_id, elapsed_ms, method, url, "RTSP request");
        } else {
            tracing::info!(connection_id, elapsed_ms, method, url, "RTSP request");
        }
        let resp = rtsp::dispatch(&mut self.conn, request);

        // Queue encryption activation for AFTER this response is sent
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = &self.conn.ap2_shared_secret
        {
            self.pending_secret = Some(secret.clone());
        }

        resp
    }

    fn is_encrypted(&self) -> bool {
        #[cfg(feature = "ap2")]
        {
            self.cipher.is_some()
        }
        #[cfg(not(feature = "ap2"))]
        {
            false
        }
    }

    fn after_response(&mut self) {
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = self.pending_secret.take()
        {
            tracing::debug!(secret_len = secret.len(), "Activating cipher from pending_secret");
            match crate::crypto::chacha_transport::EncryptedChannel::control(&secret) {
                Ok(ch) => {
                    tracing::info!("Encrypted RTSP transport activated");
                    self.cipher = Some(ch);
                }
                Err(e) => tracing::warn!("Failed to create cipher: {e}"),
            }
        }
    }

    fn decrypt_incoming(&mut self, data: &[u8]) -> Option<(Vec<u8>, usize)> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            return ch.decrypt_ctx.decrypt(data).ok();
        }
        Some((data.to_vec(), data.len()))
    }

    fn encrypt_outgoing(&mut self, data: &[u8]) -> Vec<u8> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            // Once the channel is encrypted the peer expects ciphertext; never
            // fall back to emitting plaintext (which would leak the response and
            // desync the stream). On the practically-impossible AEAD encrypt
            // failure, return no bytes so the connection tears down instead.
            return ch.encrypt_ctx.encrypt(data).unwrap_or_else(|e| {
                tracing::warn!("Outgoing encryption failed; dropping response: {e}");
                Vec::new()
            });
        }
        data.to_vec()
    }
}

// On drop, RTP session is cleaned up automatically (RaopRtp dropped → shutdown sent)
