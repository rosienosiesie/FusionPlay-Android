use crate::events::{CoreEvent, EventSink};
use crate::takeover::{MediaLease, MediaSource, PlaybackArbiter};
use anyhow::{Context, Result, anyhow, bail};
use rand::RngCore;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const SSDP_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SSDP_MAX_AGE_SECONDS: u32 = 1_800;
const HTTP_REQUEST_LIMIT: usize = 512 * 1024;
const HTTP_HEADER_LIMIT: usize = 64 * 1024;
const MEDIA_URI_LIMIT: usize = 8 * 1024;
const SERVER_HEADER: &str = "Windows/10.0 UPnP/1.0 AirPlayReceiver/0.1";

const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AV_TRANSPORT_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_TYPE: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const CONNECTION_MANAGER_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

const SINK_PROTOCOL_INFO: &str = concat!(
    "http-get:*:audio/mpeg:*,",
    "http-get:*:audio/mp4:*,",
    "http-get:*:audio/aac:*,",
    "http-get:*:audio/flac:*,",
    "http-get:*:audio/wav:*,",
    "http-get:*:audio/x-wav:*,",
    "http-get:*:video/mp4:*,",
    "http-get:*:video/mpeg:*,",
    "http-get:*:video/x-matroska:*,",
    "http-get:*:application/vnd.apple.mpegurl:*,",
    "http-get:*:application/x-mpegURL:*"
);

pub struct DmrService {
    controller: Arc<DmrController>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl DmrService {
    pub async fn start(
        name: String,
        device_key: [u8; 6],
        events: Arc<EventSink>,
        arbiter: Arc<PlaybackArbiter>,
    ) -> Result<Self> {
        let http_listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("无法监听 DLNA HTTP 服务")?;
        let http_port = http_listener
            .local_addr()
            .context("无法读取 DLNA HTTP 端口")?
            .port();
        let interface_addresses = interface_ipv4_addresses().context("无法枚举 SSDP 网络接口")?;
        let ssdp_socket =
            create_ssdp_socket(&interface_addresses).context("无法监听 SSDP 1900 端口")?;
        let device_uuid = stable_device_uuid(device_key);
        let udn = format!("uuid:{device_uuid}");
        let gena = Arc::new(GenaHub::default());
        let controller = Arc::new(DmrController::new(
            Arc::clone(&events),
            Arc::clone(&gena),
            Arc::clone(&arbiter),
        ));
        let weak_controller = Arc::downgrade(&controller);
        arbiter.register_suspender(MediaSource::Dlna, move |lease| {
            if let Some(controller) = weak_controller.upgrade() {
                controller.pause_for_takeover(lease);
            }
        });
        let context = Arc::new(DmrContext {
            name,
            udn,
            http_port,
            interface_addresses,
            controller: Arc::clone(&controller),
            gena,
        });
        let (shutdown, shutdown_receiver) = watch::channel(false);

        let http_context = Arc::clone(&context);
        let http_shutdown = shutdown_receiver.clone();
        let http_task = tokio::spawn(async move {
            if let Err(error) = run_http_server(http_listener, http_context, http_shutdown).await {
                tracing::warn!("DLNA HTTP 服务已退出：{error:#}");
            }
        });

        let ssdp_context = Arc::clone(&context);
        let ssdp_task = tokio::spawn(async move {
            if let Err(error) = run_ssdp_server(ssdp_socket, ssdp_context, shutdown_receiver).await
            {
                tracing::warn!("DLNA SSDP 服务已退出：{error:#}");
            }
        });

        events.emit(CoreEvent::DlnaReady {
            port: http_port,
            device_uuid: &device_uuid,
        });

        Ok(Self {
            controller,
            shutdown,
            tasks: vec![http_task, ssdp_task],
        })
    }

    pub fn controller(&self) -> Arc<DmrController> {
        Arc::clone(&self.controller)
    }

    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for DmrService {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

struct DmrContext {
    name: String,
    udn: String,
    http_port: u16,
    interface_addresses: Vec<Ipv4Addr>,
    controller: Arc<DmrController>,
    gena: Arc<GenaHub>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportState {
    NoMediaPresent,
    Stopped,
    Transitioning,
    Playing,
    PausedPlayback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueDirection {
    Previous,
    Next,
}

impl TransportState {
    fn as_upnp(self) -> &'static str {
        match self {
            Self::NoMediaPresent => "NO_MEDIA_PRESENT",
            Self::Stopped => "STOPPED",
            Self::Transitioning => "TRANSITIONING",
            Self::Playing => "PLAYING",
            Self::PausedPlayback => "PAUSED_PLAYBACK",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct MediaMetadata {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    artwork_url: Option<String>,
    mime_type: Option<String>,
    bitrate_bps: Option<u64>,
    sample_rate: Option<u32>,
    bits_per_sample: Option<u16>,
    channels: Option<u16>,
    upnp_class: Option<String>,
    duration_ms: Option<u64>,
    lyrics_text: Option<String>,
    lyrics_uri: Option<String>,
    raw: String,
}

#[derive(Debug)]
struct DmrState {
    current_uri: Option<String>,
    previous_uri: Option<String>,
    next_uri: Option<String>,
    owner_peer: Option<IpAddr>,
    metadata: MediaMetadata,
    previous_metadata: String,
    next_metadata: String,
    transport_state: TransportState,
    position_ms: u64,
    duration_ms: u64,
    rate: f32,
    ready: bool,
    renderer_active: bool,
    lease: Option<MediaLease>,
    last_update: Instant,
    volume: u8,
    muted: bool,
}

impl Default for DmrState {
    fn default() -> Self {
        Self {
            current_uri: None,
            previous_uri: None,
            next_uri: None,
            owner_peer: None,
            metadata: MediaMetadata::default(),
            previous_metadata: String::new(),
            next_metadata: String::new(),
            transport_state: TransportState::NoMediaPresent,
            position_ms: 0,
            duration_ms: 0,
            rate: 0.0,
            ready: false,
            renderer_active: false,
            lease: None,
            last_update: Instant::now(),
            volume: 100,
            muted: false,
        }
    }
}

impl DmrState {
    fn settle_clock(&mut self) {
        if self.transport_state == TransportState::Playing && self.rate > 0.0 {
            let elapsed_ms = (self.last_update.elapsed().as_secs_f64() * 1_000.0 * self.rate as f64)
                .round()
                .max(0.0) as u64;
            self.position_ms = self.position_ms.saturating_add(elapsed_ms);
            if self.duration_ms > 0 {
                self.position_ms = self.position_ms.min(self.duration_ms);
            }
        }
        self.last_update = Instant::now();
    }

    fn current_position_ms(&self) -> u64 {
        if self.transport_state != TransportState::Playing || self.rate <= 0.0 {
            return self.position_ms;
        }
        let elapsed_ms = (self.last_update.elapsed().as_secs_f64() * 1_000.0 * self.rate as f64)
            .round()
            .max(0.0) as u64;
        let position = self.position_ms.saturating_add(elapsed_ms);
        if self.duration_ms > 0 {
            position.min(self.duration_ms)
        } else {
            position
        }
    }
}

/// Shared DLNA transport state used by SOAP requests and the JSON UI bridge.
pub struct DmrController {
    state: Mutex<DmrState>,
    events: Arc<EventSink>,
    gena: Arc<GenaHub>,
    arbiter: Arc<PlaybackArbiter>,
}

impl DmrController {
    fn new(events: Arc<EventSink>, gena: Arc<GenaHub>, arbiter: Arc<PlaybackArbiter>) -> Self {
        Self {
            state: Mutex::new(DmrState::default()),
            events,
            gena,
            arbiter,
        }
    }

    pub fn update_playback_state(
        &self,
        position_ms: Option<u64>,
        duration_ms: Option<u64>,
        rate: Option<f32>,
        ready: Option<bool>,
    ) {
        let should_notify = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if !state.renderer_active
                || !state
                    .lease
                    .is_some_and(|lease| self.arbiter.is_current(lease))
            {
                return;
            }

            let previous_state = state.transport_state;
            let previous_duration = state.duration_ms;
            state.settle_clock();
            if let Some(position_ms) = position_ms {
                state.position_ms = position_ms;
            }
            if let Some(duration_ms) = duration_ms {
                state.duration_ms = duration_ms;
            }
            if let Some(ready) = ready {
                state.ready = ready;
            }
            if let Some(rate) = rate.filter(|value| value.is_finite()) {
                state.rate = rate.max(0.0);
                state.transport_state = if !state.ready {
                    TransportState::Transitioning
                } else if state.rate > 0.0 {
                    TransportState::Playing
                } else {
                    TransportState::PausedPlayback
                };
            }
            state.last_update = Instant::now();
            previous_state != state.transport_state || previous_duration != state.duration_ms
        };
        if should_notify {
            self.notify_transport();
        }
    }

    /// Handles commands that target the locally rendered DLNA session.
    ///
    /// `None` means that DLNA is not active and the caller should preserve the
    /// existing AirPlay remote-control path.
    pub fn handle_ui_command(&self, command: &str, position_ms: Option<u64>) -> Option<Result<()>> {
        let selected = self
            .state
            .lock()
            .map(|state| {
                state.current_uri.is_some()
                    && state
                        .lease
                        .is_some_and(|lease| self.arbiter.is_current(lease))
            })
            .unwrap_or(false);
        if !selected {
            return None;
        }

        let result = match command {
            "play" => self.play(),
            "pause" => self.pause(),
            "play_pause" => {
                let playing = self
                    .state
                    .lock()
                    .map(|state| state.transport_state == TransportState::Playing)
                    .unwrap_or(false);
                if playing { self.pause() } else { self.play() }
            }
            "seek" => position_ms
                .ok_or_else(|| anyhow!("seek 命令缺少 position_ms"))
                .and_then(|position| self.seek(position)),
            "set_volume" => position_ms
                .ok_or_else(|| anyhow!("set_volume 命令缺少 position_ms"))
                .and_then(|percent| self.set_volume(percent.min(100) as u8)),
            "previous_track" => self.previous_track(),
            "next_track" => self.next_track(),
            _ => return None,
        };
        Some(result)
    }

    #[cfg(test)]
    fn set_transport_uri(&self, uri: &str, metadata: &str) -> Result<()> {
        self.set_transport_uri_from(uri, metadata, None)
    }

    fn set_transport_uri_from(
        &self,
        uri: &str,
        metadata: &str,
        peer: Option<IpAddr>,
    ) -> Result<()> {
        if uri.is_empty() {
            return self.clear_transport_uri();
        }
        validate_media_uri(uri)?;
        let parsed_metadata = parse_didl_metadata_for_uri(metadata, Some(uri));
        /*
         * Several DLNA control points repeat SetAVTransportURI while resuming a
         * paused/stopped item. The URI identifies the resource; DIDL is mutable
         * descriptive data and frequently changes only in whitespace, signed
         * artwork tokens, or vendor timestamps. Therefore every request for
         * the selected URI is an in-place metadata refresh. Position and
         * transport state are preserved.
         *
         * An empty metadata argument must not discard existing metadata. A
         * changed URI still selects a new resource and starts at zero.
         *
         * The same URI also survives a control-point reconnect (the sender's
         * source port or process may have changed). Ownership follows the
         * latest SetAVTransportURI request, while an explicit Seek remains the
         * only way to reset that selected resource to zero.
         *
         * Resource selection alone must not preempt another playback source.
         * DLNA claims the shared renderer only when the owner sends Play.
         */
        let refreshed_metadata = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA playback state is unavailable"))?;
            let refreshes_selected_resource = state.current_uri.as_deref() == Some(uri);
            if refreshes_selected_resource {
                if !metadata.trim().is_empty() {
                    if let Some(duration_ms) = parsed_metadata.duration_ms {
                        state.duration_ms = duration_ms;
                    }
                    state.metadata = parsed_metadata.clone();
                }
                if peer.is_some() {
                    state.owner_peer = peer;
                }
                let active_lease = state
                    .renderer_active
                    .then_some(state.lease.filter(|lease| self.arbiter.is_current(*lease)))
                    .flatten();
                Some((state.metadata.clone(), state.position_ms, active_lease))
            } else {
                None
            }
        };
        if let Some((metadata, position_ms, active_lease)) = refreshed_metadata {
            if let Some(lease) = active_lease {
                self.emit_selected_media(lease, uri, &metadata, position_ms);
            }
            self.notify_transport();
            self.notify_connection();
            return Ok(());
        }
        let (previous_lease, stopped_active) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            let previous_lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
            let stopped_active = previous_lease.is_some() && state.renderer_active;
            state.settle_clock();
            if let Some(previous_uri) = state.current_uri.replace(uri.to_owned()) {
                state.previous_uri = Some(previous_uri);
                state.previous_metadata = state.metadata.raw.clone();
            }
            if state.next_uri.as_deref() == Some(uri) {
                state.next_uri = None;
                state.next_metadata.clear();
            }
            state.owner_peer = peer;
            state.metadata = parsed_metadata;
            state.transport_state = TransportState::Stopped;
            state.position_ms = 0;
            state.duration_ms = state.metadata.duration_ms.unwrap_or(0);
            state.rate = 0.0;
            state.ready = false;
            state.renderer_active = false;
            state.lease = None;
            state.last_update = Instant::now();
            (previous_lease, stopped_active)
        };

        if let Some(lease) = previous_lease {
            if stopped_active {
                self.events.emit(CoreEvent::DlnaStop {
                    source: "dlna",
                    epoch: lease.epoch(),
                });
                self.events.emit(CoreEvent::PlaybackState {
                    source: "dlna",
                    epoch: lease.epoch(),
                    playing: false,
                });
                self.events.emit(CoreEvent::RemoteControlUnavailable {
                    source: Some("dlna"),
                    epoch: Some(lease.epoch()),
                    reason: "DLNA selected a new media resource",
                });
            }
            self.arbiter.release(lease);
        }
        self.notify_transport();
        self.notify_connection();
        Ok(())
    }

    fn ensure_owner(&self, peer: IpAddr) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("DLNA playback state is unavailable"))?;
        if state.current_uri.is_none() {
            bail!("DLNA media URI has not been set");
        }
        if state.owner_peer.is_some_and(|owner| owner != peer) {
            bail!("DLNA command belongs to a different control point");
        }
        Ok(())
    }

    fn owned_current_lease(&self, peer: IpAddr) -> Result<MediaLease> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("DLNA playback state is unavailable"))?;
        if state.owner_peer.is_some_and(|owner| owner != peer) {
            bail!("DLNA command belongs to a different control point");
        }
        state
            .lease
            .filter(|lease| self.arbiter.is_current(*lease))
            .ok_or_else(|| anyhow!("DLNA is not the active playback source"))
    }

    fn clear_transport_uri(&self) -> Result<()> {
        let current_lease = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.lease)
            .filter(|lease| self.arbiter.is_current(*lease));
        let Some(lease) = current_lease else {
            return self.clear_transport_uri_inner();
        };
        self.arbiter
            .finish_if_current(lease, || self.clear_transport_uri_inner())
            .unwrap_or_else(|| self.clear_transport_uri_inner())
    }

    fn clear_transport_uri_inner(&self) -> Result<()> {
        let (was_active, lease) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            let was_active = state.renderer_active;
            let lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
            *state = DmrState {
                volume: state.volume,
                muted: state.muted,
                ..DmrState::default()
            };
            (was_active, lease)
        };
        if was_active && let Some(lease) = lease {
            self.events.emit(CoreEvent::DlnaStop {
                source: "dlna",
                epoch: lease.epoch(),
            });
        }
        if let Some(lease) = lease {
            self.events.emit(CoreEvent::NowPlaying {
                source: "dlna",
                epoch: lease.epoch(),
                title: None,
                artist: None,
                album: None,
                genre: None,
                duration_ms: None,
            });
            self.events.emit(CoreEvent::PlaybackState {
                source: "dlna",
                epoch: lease.epoch(),
                playing: false,
            });
            self.events.emit(CoreEvent::RemoteControlUnavailable {
                source: Some("dlna"),
                epoch: Some(lease.epoch()),
                reason: "DLNA 媒体地址已清除",
            });
            self.arbiter.release(lease);
        }
        self.notify_transport();
        self.notify_connection();
        Ok(())
    }

    fn set_next_transport_uri(&self, uri: &str, metadata: &str) -> Result<()> {
        if !uri.is_empty() {
            validate_media_uri(uri)?;
        }
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            state.next_uri = (!uri.is_empty()).then(|| uri.to_owned());
            state.next_metadata = if uri.is_empty() {
                String::new()
            } else {
                metadata.to_owned()
            };
        }
        self.notify_transport();
        self.emit_remote_control_available();
        Ok(())
    }

    fn set_next_transport_uri_from(&self, peer: IpAddr, uri: &str, metadata: &str) -> Result<()> {
        self.ensure_owner(peer)?;
        self.set_next_transport_uri(uri, metadata)
    }

    fn previous_track(&self) -> Result<()> {
        self.select_adjacent_track(QueueDirection::Previous, None)
    }

    fn previous_track_from(&self, peer: IpAddr) -> Result<()> {
        self.ensure_owner(peer)?;
        self.select_adjacent_track(QueueDirection::Previous, Some(peer))
    }

    fn next_track(&self) -> Result<()> {
        self.select_adjacent_track(QueueDirection::Next, None)
    }

    fn next_track_from(&self, peer: IpAddr) -> Result<()> {
        self.ensure_owner(peer)?;
        self.select_adjacent_track(QueueDirection::Next, Some(peer))
    }

    /// Selects a queued URI without manufacturing a teardown/reconnect.
    ///
    /// A different URI starts at zero and is published under the current DLNA
    /// lease, allowing the local decoder to replace the resource atomically.
    /// A repeated URI is an in-place metadata refresh/resume so a sender that
    /// mirrors SetAVTransportURI and Next/Previous cannot erase its position.
    fn select_adjacent_track(&self, direction: QueueDirection, peer: Option<IpAddr>) -> Result<()> {
        let paused_lease = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            if peer
                .is_some_and(|requester| state.owner_peer.is_some_and(|owner| owner != requester))
            {
                bail!("DLNA command belongs to a different control point");
            }
            let current_uri = state
                .current_uri
                .clone()
                .ok_or_else(|| anyhow!("DLNA media URI has not been set"))?;
            let (target_uri, target_metadata) = match direction {
                QueueDirection::Previous => (
                    state
                        .previous_uri
                        .take()
                        .ok_or_else(|| anyhow!("没有可用的上一首 DLNA 媒体"))?,
                    std::mem::take(&mut state.previous_metadata),
                ),
                QueueDirection::Next => (
                    state
                        .next_uri
                        .take()
                        .ok_or_else(|| anyhow!("没有可用的下一首 DLNA 媒体"))?,
                    std::mem::take(&mut state.next_metadata),
                ),
            };
            if let Some(peer) = peer {
                state.owner_peer = Some(peer);
            }

            if target_uri == current_uri {
                if !target_metadata.trim().is_empty() {
                    let parsed_metadata =
                        parse_didl_metadata_for_uri(&target_metadata, Some(&target_uri));
                    if let Some(duration_ms) = parsed_metadata.duration_ms {
                        state.duration_ms = duration_ms;
                    }
                    state.metadata = parsed_metadata;
                }
                None
            } else {
                state.settle_clock();
                let current_metadata = state.metadata.raw.clone();
                match direction {
                    QueueDirection::Previous => {
                        state.next_uri = Some(current_uri);
                        state.next_metadata = current_metadata;
                    }
                    QueueDirection::Next => {
                        state.previous_uri = Some(current_uri);
                        state.previous_metadata = current_metadata;
                    }
                }
                let parsed_target_metadata =
                    parse_didl_metadata_for_uri(&target_metadata, Some(&target_uri));
                state.current_uri = Some(target_uri);
                state.metadata = parsed_target_metadata;
                state.position_ms = 0;
                state.duration_ms = state.metadata.duration_ms.unwrap_or(0);
                state.rate = 0.0;
                state.ready = false;
                state.transport_state = TransportState::Stopped;
                state.last_update = Instant::now();
                let lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
                if lease.is_none() {
                    state.lease = None;
                }
                let paused_lease = state.renderer_active.then_some(lease).flatten();
                state.renderer_active = false;
                paused_lease
            }
        };

        if let Some(lease) = paused_lease {
            self.events.emit(CoreEvent::DlnaRate {
                source: "dlna",
                epoch: lease.epoch(),
                rate: 0.0,
            });
            self.events.emit(CoreEvent::PlaybackState {
                source: "dlna",
                epoch: lease.epoch(),
                playing: false,
            });
        }
        self.notify_transport();
        self.play_selected(peer)
    }

    fn play_selected(&self, peer: Option<IpAddr>) -> Result<()> {
        let (selected_media_kind, active_lease, selected_uri) = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA playback state is unavailable"))?;
            if peer.is_some_and(|peer| state.owner_peer.is_some_and(|owner| owner != peer)) {
                bail!("DLNA command belongs to a different control point");
            }
            let uri = state
                .current_uri
                .as_deref()
                .ok_or_else(|| anyhow!("DLNA media URI has not been set"))?;
            let selected_media_kind = media_kind(
                uri,
                state.metadata.mime_type.as_deref(),
                state.metadata.upnp_class.as_deref(),
            );
            // An ordinary Pause keeps the current lease. A first Play or a
            // Play that resumes a cross-source suspension claims a new one.
            let active_lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
            (selected_media_kind, active_lease, uri.to_owned())
        };
        let (lease, _transition) = if let Some(lease) = active_lease {
            (lease, None)
        } else {
            let (lease, transition) = self
                .arbiter
                .begin_takeover_checked(
                    MediaSource::Dlna,
                    selected_media_kind,
                    "dlna_play",
                    false,
                    || {
                        self.state.lock().is_ok_and(|state| {
                            state.current_uri.as_deref() == Some(selected_uri.as_str())
                                && !peer.is_some_and(|peer| {
                                    state.owner_peer.is_some_and(|owner| owner != peer)
                                })
                        })
                    },
                )
                .ok_or_else(|| anyhow!("DLNA media selection changed before Play"))?;
            (lease, Some(transition))
        };
        let (emit_media, uri, metadata, position_ms) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            let uri = state
                .current_uri
                .clone()
                .ok_or_else(|| anyhow!("尚未设置 DLNA 媒体地址"))?;
            state.settle_clock();
            let emit_media = !state.renderer_active;
            state.renderer_active = true;
            state.lease = Some(lease);
            state.transport_state = if state.ready {
                TransportState::Playing
            } else {
                TransportState::Transitioning
            };
            state.rate = 1.0;
            state.last_update = Instant::now();
            (emit_media, uri, state.metadata.clone(), state.position_ms)
        };

        if emit_media {
            self.emit_selected_media(lease, &uri, &metadata, position_ms);
        }
        self.emit_remote_control_available();
        self.events.emit(CoreEvent::DlnaRate {
            source: "dlna",
            epoch: lease.epoch(),
            rate: 1.0,
        });
        self.events.emit(CoreEvent::PlaybackState {
            source: "dlna",
            epoch: lease.epoch(),
            playing: true,
        });
        self.notify_transport();
        Ok(())
    }

    fn emit_selected_media(
        &self,
        lease: MediaLease,
        uri: &str,
        metadata: &MediaMetadata,
        position_ms: u64,
    ) {
        let media_kind = media_kind(
            uri,
            metadata.mime_type.as_deref(),
            metadata.upnp_class.as_deref(),
        );
        self.events.emit(CoreEvent::DlnaMedia {
            source: "dlna",
            epoch: lease.epoch(),
            url: uri,
            title: metadata.title.as_deref(),
            artist: metadata.artist.as_deref(),
            album: metadata.album.as_deref(),
            artwork_url: metadata.artwork_url.as_deref(),
            content_type: metadata.mime_type.as_deref(),
            bitrate_bps: metadata.bitrate_bps,
            sample_rate: metadata.sample_rate,
            bits_per_sample: metadata.bits_per_sample,
            channels: metadata.channels,
            upnp_class: metadata.upnp_class.as_deref(),
            media_kind,
            duration_ms: metadata.duration_ms,
            start_position_ms: position_ms,
            lyrics_text: metadata.lyrics_text.as_deref(),
            lyrics_uri: metadata.lyrics_uri.as_deref(),
        });
        self.events.emit(CoreEvent::NowPlaying {
            source: "dlna",
            epoch: lease.epoch(),
            title: metadata.title.as_deref(),
            artist: metadata.artist.as_deref(),
            album: metadata.album.as_deref(),
            genre: None,
            duration_ms: metadata
                .duration_ms
                .and_then(|value| u32::try_from(value).ok()),
        });
    }

    fn play(&self) -> Result<()> {
        self.play_selected(None)
    }

    fn play_from(&self, peer: IpAddr) -> Result<()> {
        self.play_selected(Some(peer))
    }

    fn emit_remote_control_available(&self) {
        let available = self.state.lock().ok().and_then(|state| {
            let lease = state
                .lease
                .filter(|lease| self.arbiter.is_current(*lease))?;
            state.current_uri.as_ref()?;
            let mut commands = vec!["play", "pause", "play_pause", "seek"];
            if state.previous_uri.is_some() {
                commands.push("previous_track");
            }
            if state.next_uri.is_some() {
                commands.push("next_track");
            }
            Some((lease, commands))
        });
        if let Some((lease, commands)) = available {
            self.events.emit(CoreEvent::RemoteControlAvailable {
                source: "dlna",
                epoch: lease.epoch(),
                commands,
                transport: "dlna",
                experimental: false,
            });
        }
    }

    fn pause(&self) -> Result<()> {
        let lease = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            if !state.renderer_active {
                bail!("当前没有活动的 DLNA 播放");
            }
            let lease = state
                .lease
                .filter(|lease| self.arbiter.is_current(*lease))
                .ok_or_else(|| anyhow!("DLNA is not the active playback source"))?;
            state.settle_clock();
            state.rate = 0.0;
            state.transport_state = TransportState::PausedPlayback;
            lease
        };
        self.events.emit(CoreEvent::DlnaRate {
            source: "dlna",
            epoch: lease.epoch(),
            rate: 0.0,
        });
        self.events.emit(CoreEvent::PlaybackState {
            source: "dlna",
            epoch: lease.epoch(),
            playing: false,
        });
        self.notify_transport();
        Ok(())
    }

    fn pause_from(&self, peer: IpAddr) -> Result<()> {
        let lease = self.owned_current_lease(peer)?;
        self.arbiter
            .run_if_current(lease, || self.pause())
            .ok_or_else(|| anyhow!("DLNA is not the active playback source"))?
    }

    #[cfg(test)]
    fn stop(&self) -> Result<()> {
        self.stop_inner()
    }

    fn stop_from(&self, peer: IpAddr) -> Result<()> {
        self.ensure_owner(peer)?;
        self.stop_inner()
    }

    fn stop_inner(&self) -> Result<()> {
        let (should_emit, lease) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            state.settle_clock();
            state.transport_state = if state.current_uri.is_some() {
                TransportState::Stopped
            } else {
                TransportState::NoMediaPresent
            };
            state.rate = 0.0;
            state.ready = false;
            let lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
            if lease.is_none() {
                state.lease = None;
            }
            (std::mem::take(&mut state.renderer_active), lease)
        };
        if should_emit && let Some(lease) = lease {
            self.events.emit(CoreEvent::DlnaStop {
                source: "dlna",
                epoch: lease.epoch(),
            });
        }
        if let Some(lease) = lease {
            self.events.emit(CoreEvent::PlaybackState {
                source: "dlna",
                epoch: lease.epoch(),
                playing: false,
            });
        }
        if lease.is_some() {
            self.emit_remote_control_available();
        }
        self.notify_transport();
        Ok(())
    }

    fn pause_for_takeover(&self, lease: MediaLease) {
        let had_session = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.lease != Some(lease) {
                return;
            }
            state.settle_clock();
            let had_session = state.current_uri.is_some() || state.renderer_active;
            if had_session {
                state.transport_state = TransportState::PausedPlayback;
                state.rate = 0.0;
                state.ready = false;
                state.renderer_active = false;
                state.lease = None;
                state.last_update = Instant::now();
            }
            had_session
        };
        if had_session {
            self.events.emit(CoreEvent::DlnaRate {
                source: "dlna",
                epoch: lease.epoch(),
                rate: 0.0,
            });
            self.events.emit(CoreEvent::PlaybackState {
                source: "dlna",
                epoch: lease.epoch(),
                playing: false,
            });
        }
        self.notify_transport();
        self.notify_connection();
    }

    fn seek(&self, position_ms: u64) -> Result<()> {
        self.seek_inner(position_ms)
    }

    fn seek_from(&self, peer: IpAddr, position_ms: u64) -> Result<()> {
        self.ensure_owner(peer)?;
        self.seek_inner(position_ms)
    }

    fn seek_inner(&self, position_ms: u64) -> Result<()> {
        let (position_ms, duration_ms, lease) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
            if state.current_uri.is_none() {
                bail!("尚未设置 DLNA 媒体地址");
            }
            let lease = state.lease.filter(|lease| self.arbiter.is_current(*lease));
            state.position_ms = if state.duration_ms > 0 {
                position_ms.min(state.duration_ms)
            } else {
                position_ms
            };
            state.last_update = Instant::now();
            (state.position_ms, state.duration_ms, lease)
        };
        if let Some(lease) = lease {
            self.events.emit(CoreEvent::DlnaSeek {
                source: "dlna",
                epoch: lease.epoch(),
                position_ms,
            });
            self.events.emit(CoreEvent::Progress {
                source: "dlna",
                epoch: lease.epoch(),
                position_ms,
                duration_ms,
            });
        }
        self.notify_transport();
        Ok(())
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        let (muted, lease) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 音量状态不可用"))?;
            state.volume = volume.min(100);
            (
                state.muted,
                state.lease.filter(|lease| self.arbiter.is_current(*lease)),
            )
        };
        if let Some(lease) = lease {
            self.events.emit(CoreEvent::DlnaVolume {
                source: "dlna",
                epoch: lease.epoch(),
                percent: volume.min(100),
                muted,
            });
        }
        self.notify_rendering();
        Ok(())
    }

    fn set_muted(&self, muted: bool) -> Result<()> {
        let (volume, lease) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("DLNA 音量状态不可用"))?;
            state.muted = muted;
            (
                state.volume,
                state.lease.filter(|lease| self.arbiter.is_current(*lease)),
            )
        };
        if let Some(lease) = lease {
            self.events.emit(CoreEvent::DlnaVolume {
                source: "dlna",
                epoch: lease.epoch(),
                percent: volume,
                muted,
            });
        }
        self.notify_rendering();
        Ok(())
    }

    fn snapshot(&self) -> Result<DmrSnapshot> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("DLNA 播放状态不可用"))?;
        Ok(DmrSnapshot {
            current_uri: state.current_uri.clone().unwrap_or_default(),
            previous_uri: state.previous_uri.clone().unwrap_or_default(),
            next_uri: state.next_uri.clone().unwrap_or_default(),
            metadata: state.metadata.clone(),
            next_metadata: state.next_metadata.clone(),
            transport_state: state.transport_state,
            position_ms: state.current_position_ms(),
            duration_ms: state.duration_ms,
            volume: state.volume,
            muted: state.muted,
        })
    }

    fn notify_transport(&self) {
        if let Ok(snapshot) = self.snapshot() {
            self.gena
                .notify(ServiceKind::AvTransport, transport_event_body(&snapshot));
        }
    }

    fn notify_rendering(&self) {
        if let Ok(snapshot) = self.snapshot() {
            self.gena.notify(
                ServiceKind::RenderingControl,
                rendering_event_body(&snapshot),
            );
        }
    }

    fn notify_connection(&self) {
        if let Ok(snapshot) = self.snapshot() {
            self.gena.notify(
                ServiceKind::ConnectionManager,
                connection_event_body(&snapshot),
            );
        }
    }
}

#[derive(Debug)]
struct DmrSnapshot {
    current_uri: String,
    previous_uri: String,
    next_uri: String,
    metadata: MediaMetadata,
    next_metadata: String,
    transport_state: TransportState,
    position_ms: u64,
    duration_ms: u64,
    volume: u8,
    muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ServiceKind {
    AvTransport,
    RenderingControl,
    ConnectionManager,
}

#[derive(Debug, Clone)]
struct CallbackUrl {
    host: String,
    port: u16,
    path_and_query: String,
}

#[derive(Debug, Clone)]
struct GenaSubscription {
    service: ServiceKind,
    callback: CallbackUrl,
    expires_at: Instant,
    timeout_seconds: u64,
    sequence: u32,
}

#[derive(Default)]
struct GenaHub {
    subscriptions: Mutex<HashMap<String, GenaSubscription>>,
}

impl GenaHub {
    fn subscribe(
        self: &Arc<Self>,
        service: ServiceKind,
        request: &HttpRequest,
        peer: SocketAddr,
        initial_body: String,
    ) -> std::result::Result<(String, u64), &'static str> {
        let timeout_seconds = parse_gena_timeout(request.headers.get("timeout"));
        if let Some(sid) = request.headers.get("sid") {
            if request.headers.contains_key("callback") || request.headers.contains_key("nt") {
                return Err("续订请求不能同时包含 CALLBACK 或 NT");
            }
            let mut subscriptions = self.subscriptions.lock().map_err(|_| "订阅状态不可用")?;
            let sid = sid.trim();
            if subscriptions
                .get(sid)
                .is_some_and(|subscription| subscription.expires_at <= Instant::now())
            {
                subscriptions.remove(sid);
            }
            let Some(subscription) = subscriptions.get_mut(sid) else {
                return Err("未知或已过期的 SID");
            };
            if subscription.service != service {
                return Err("SID 与事件服务不匹配");
            }
            subscription.timeout_seconds = timeout_seconds;
            subscription.expires_at = Instant::now() + Duration::from_secs(timeout_seconds);
            return Ok((sid.to_owned(), timeout_seconds));
        }

        if !request
            .headers
            .get("nt")
            .is_some_and(|value| value.eq_ignore_ascii_case("upnp:event"))
        {
            return Err("新订阅缺少 NT: upnp:event");
        }
        let callback = request
            .headers
            .get("callback")
            .ok_or("新订阅缺少 CALLBACK")
            .and_then(|value| parse_callback_url(value, peer))?;
        let sid = generate_subscription_id();
        self.subscriptions
            .lock()
            .map_err(|_| "订阅状态不可用")?
            .insert(
                sid.clone(),
                GenaSubscription {
                    service,
                    callback,
                    expires_at: Instant::now() + Duration::from_secs(timeout_seconds),
                    timeout_seconds,
                    sequence: 0,
                },
            );
        self.notify_sid_after_delay(sid.clone(), initial_body);
        Ok((sid, timeout_seconds))
    }

    fn unsubscribe(
        &self,
        service: ServiceKind,
        request: &HttpRequest,
    ) -> std::result::Result<(), &'static str> {
        if request.headers.contains_key("callback") || request.headers.contains_key("nt") {
            return Err("取消订阅请求不能包含 CALLBACK 或 NT");
        }
        let sid = request
            .headers
            .get("sid")
            .map(|value| value.trim())
            .ok_or("取消订阅缺少 SID")?;
        let mut subscriptions = self.subscriptions.lock().map_err(|_| "订阅状态不可用")?;
        if subscriptions
            .get(sid)
            .is_none_or(|subscription| subscription.service != service)
        {
            return Err("未知或不匹配的 SID");
        }
        subscriptions.remove(sid);
        Ok(())
    }

    fn notify(self: &Arc<Self>, service: ServiceKind, body: String) {
        let pending = {
            let Ok(mut subscriptions) = self.subscriptions.lock() else {
                return;
            };
            let now = Instant::now();
            subscriptions.retain(|_, subscription| subscription.expires_at > now);
            subscriptions
                .iter_mut()
                .filter(|(_, subscription)| subscription.service == service)
                .map(|(sid, subscription)| {
                    let sequence = subscription.sequence;
                    subscription.sequence = subscription.sequence.wrapping_add(1);
                    (
                        sid.clone(),
                        subscription.callback.clone(),
                        sequence,
                        body.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (sid, callback, sequence, body) in pending {
            tokio::spawn(async move {
                if let Err(error) = send_gena_notify(&callback, &sid, sequence, &body).await {
                    tracing::debug!("GENA NOTIFY {sid} 失败：{error:#}");
                }
            });
        }
    }

    fn notify_sid_after_delay(self: &Arc<Self>, sid: String, body: String) {
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            // Deliver the mandatory initial event after the SUBSCRIBE response
            // has had a chance to reach the control point.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let pending = {
                let Ok(mut subscriptions) = hub.subscriptions.lock() else {
                    return;
                };
                let Some(subscription) = subscriptions.get_mut(&sid) else {
                    return;
                };
                if subscription.expires_at <= Instant::now() {
                    subscriptions.remove(&sid);
                    return;
                }
                let sequence = subscription.sequence;
                subscription.sequence = subscription.sequence.wrapping_add(1);
                (subscription.callback.clone(), sequence)
            };
            if let Err(error) = send_gena_notify(&pending.0, &sid, pending.1, &body).await {
                tracing::debug!("GENA 初始 NOTIFY {sid} 失败：{error:#}");
            }
        });
    }
}

fn parse_callback_url(
    header: &str,
    peer: SocketAddr,
) -> std::result::Result<CallbackUrl, &'static str> {
    if header.len() > 2_048 || header.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("CALLBACK 过长或包含控制字符");
    }
    let value = header
        .trim()
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .ok_or("CALLBACK 必须使用尖括号")?;
    if value.contains("><")
        || value.contains('#')
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err("仅支持单个无片段 CALLBACK");
    }
    let remainder = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("HTTP://"))
        .ok_or("CALLBACK 仅允许 HTTP")?;
    let (authority, path) = remainder
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((remainder, "/".to_owned()));
    if authority.is_empty() || authority.contains('@') {
        return Err("CALLBACK 不允许凭据或空主机");
    }
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        let port = port.parse::<u16>().map_err(|_| "CALLBACK 端口无效")?;
        (host, port)
    } else {
        (authority, 80)
    };
    if port == 0 {
        return Err("CALLBACK 端口无效");
    }
    let callback_ip = host
        .parse::<Ipv4Addr>()
        .map_err(|_| "CALLBACK 必须使用订阅端的 IPv4 地址，不能使用域名")?;
    let SocketAddr::V4(peer) = peer else {
        return Err("当前 DLNA 服务仅接受 IPv4 订阅");
    };
    if callback_ip != *peer.ip() {
        return Err("CALLBACK 地址必须与订阅连接来源相同");
    }
    Ok(CallbackUrl {
        host: callback_ip.to_string(),
        port,
        path_and_query: path,
    })
}

fn parse_gena_timeout(header: Option<&String>) -> u64 {
    const DEFAULT: u64 = 1_800;
    const MAXIMUM: u64 = 86_400;
    let Some(value) = header.map(|value| value.trim()) else {
        return DEFAULT;
    };
    if value.eq_ignore_ascii_case("Second-infinite") {
        return MAXIMUM;
    }
    value
        .strip_prefix("Second-")
        .or_else(|| value.strip_prefix("second-"))
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .map(|seconds| seconds.clamp(30, MAXIMUM))
        .unwrap_or(DEFAULT)
}

fn generate_subscription_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "uuid:{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

async fn send_gena_notify(
    callback: &CallbackUrl,
    sid: &str,
    sequence: u32,
    body: &str,
) -> Result<()> {
    let operation = async {
        let mut stream = TcpStream::connect((callback.host.as_str(), callback.port))
            .await
            .context("无法连接 GENA CALLBACK")?;
        let request = format!(
            "NOTIFY {} HTTP/1.1\r\n\
             HOST: {}:{}\r\n\
             CONTENT-TYPE: text/xml; charset=utf-8\r\n\
             NT: upnp:event\r\n\
             NTS: upnp:propchange\r\n\
             SID: {sid}\r\n\
             SEQ: {sequence}\r\n\
             CONTENT-LENGTH: {}\r\n\
             CONNECTION: close\r\n\
             \r\n{}",
            callback.path_and_query,
            callback.host,
            callback.port,
            body.len(),
            body
        );
        stream
            .write_all(request.as_bytes())
            .await
            .context("写入 GENA NOTIFY 失败")?;
        let mut response = [0_u8; 256];
        let size = stream
            .read(&mut response)
            .await
            .context("读取 GENA NOTIFY 响应失败")?;
        let status_line = std::str::from_utf8(&response[..size])
            .ok()
            .and_then(|value| value.lines().next())
            .unwrap_or("");
        if !status_line.contains(" 200 ") {
            bail!("GENA CALLBACK 返回非 200 状态：{status_line}");
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .context("GENA CALLBACK 超时")?
}

fn initial_event_body(service: ServiceKind, controller: &DmrController) -> String {
    let snapshot = controller.snapshot().unwrap_or_else(|_| DmrSnapshot {
        current_uri: String::new(),
        previous_uri: String::new(),
        next_uri: String::new(),
        metadata: MediaMetadata::default(),
        next_metadata: String::new(),
        transport_state: TransportState::NoMediaPresent,
        position_ms: 0,
        duration_ms: 0,
        volume: 100,
        muted: false,
    });
    match service {
        ServiceKind::AvTransport => transport_event_body(&snapshot),
        ServiceKind::RenderingControl => rendering_event_body(&snapshot),
        ServiceKind::ConnectionManager => connection_event_body(&snapshot),
    }
}

fn transport_event_body(snapshot: &DmrSnapshot) -> String {
    let actions = current_transport_actions(snapshot);
    let last_change = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">\
         <InstanceID val=\"0\">\
         <TransportState val=\"{}\"/>\
         <TransportStatus val=\"OK\"/>\
         <CurrentTrackURI val=\"{}\"/>\
         <AVTransportURI val=\"{}\"/>\
         <CurrentTrackDuration val=\"{}\"/>\
         <RelativeTimePosition val=\"{}\"/>\
         <CurrentTransportActions val=\"{}\"/>\
         </InstanceID></Event>",
        snapshot.transport_state.as_upnp(),
        escape_xml(&snapshot.current_uri),
        escape_xml(&snapshot.current_uri),
        format_upnp_time(snapshot.duration_ms),
        format_upnp_time(snapshot.position_ms),
        actions
    );
    last_change_propertyset(&last_change)
}

fn current_transport_actions(snapshot: &DmrSnapshot) -> String {
    let mut actions = match snapshot.transport_state {
        TransportState::NoMediaPresent => Vec::new(),
        TransportState::Stopped => vec!["Play", "Seek"],
        TransportState::Transitioning | TransportState::Playing => {
            vec!["Stop", "Pause", "Seek"]
        }
        TransportState::PausedPlayback => vec!["Play", "Stop", "Seek"],
    };
    if !snapshot.previous_uri.is_empty() {
        actions.push("Previous");
    }
    if !snapshot.next_uri.is_empty() {
        actions.push("Next");
    }
    actions.join(",")
}

fn rendering_event_body(snapshot: &DmrSnapshot) -> String {
    let last_change = format!(
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\">\
         <InstanceID val=\"0\">\
         <Volume channel=\"Master\" val=\"{}\"/>\
         <Mute channel=\"Master\" val=\"{}\"/>\
         </InstanceID></Event>",
        snapshot.volume,
        u8::from(snapshot.muted)
    );
    last_change_propertyset(&last_change)
}

fn last_change_propertyset(last_change: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><LastChange>{}</LastChange></e:property>\
         </e:propertyset>",
        escape_xml(last_change)
    )
}

fn connection_event_body(snapshot: &DmrSnapshot) -> String {
    let connection_ids = if snapshot.current_uri.is_empty() {
        ""
    } else {
        "0"
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
         <e:property><SourceProtocolInfo></SourceProtocolInfo></e:property>\
         <e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property>\
         <e:property><CurrentConnectionIDs>{connection_ids}</CurrentConnectionIDs></e:property>\
         </e:propertyset>",
        escape_xml(SINK_PROTOCOL_INFO)
    )
}

fn interface_ipv4_addresses() -> Result<Vec<Ipv4Addr>> {
    let mut addresses = if_addrs::get_if_addrs()
        .context("GetAdaptersAddresses 失败")?
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(address)
                if !address.is_loopback() && !address.is_unspecified() =>
            {
                Some(address)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("没有可用于 DLNA 的非回环 IPv4 网络接口");
    }
    Ok(addresses)
}

fn create_ssdp_socket(interface_addresses: &[Ipv4Addr]) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("无法创建 SSDP UDP 套接字")?;
    socket
        .set_reuse_address(true)
        .context("无法启用 SSDP 地址复用")?;
    socket
        .set_multicast_ttl_v4(2)
        .context("无法设置 SSDP multicast TTL")?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT).into())
        .context("无法绑定 SSDP 组播端口")?;
    let mut joined = 0_usize;
    for interface in interface_addresses {
        match socket.join_multicast_v4(&SSDP_MULTICAST, interface) {
            Ok(()) => joined += 1,
            Err(error) => {
                tracing::debug!("无法在接口 {interface} 加入 SSDP 组播：{error}");
            }
        }
    }
    if joined == 0 {
        bail!("无法在任何 IPv4 网络接口加入 SSDP 组播组");
    }
    socket
        .set_nonblocking(true)
        .context("无法将 SSDP 套接字设为非阻塞")?;
    let socket: StdUdpSocket = socket.into();
    UdpSocket::from_std(socket).context("无法创建异步 SSDP 套接字")
}

async fn run_ssdp_server(
    socket: UdpSocket,
    context: Arc<DmrContext>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    send_ssdp_notify(&socket, &context, "ssdp:alive").await;
    let mut announce_interval =
        tokio::time::interval(Duration::from_secs((SSDP_MAX_AGE_SECONDS / 2) as u64));
    announce_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The immediate interval tick is redundant with the explicit announcement.
    announce_interval.tick().await;
    let mut buffer = [0_u8; 4_096];

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = announce_interval.tick() => {
                send_ssdp_notify(&socket, &context, "ssdp:alive").await;
            }
            received = socket.recv_from(&mut buffer) => {
                match received {
                    Ok((size, peer)) => {
                        handle_ssdp_search(&socket, &context, &buffer[..size], peer).await;
                    }
                    Err(error) => {
                        tracing::debug!("SSDP 接收失败：{error}");
                    }
                }
            }
        }
    }

    send_ssdp_notify(&socket, &context, "ssdp:byebye").await;
    Ok(())
}

async fn handle_ssdp_search(
    socket: &UdpSocket,
    context: &DmrContext,
    packet: &[u8],
    peer: SocketAddr,
) {
    let Ok(request) = std::str::from_utf8(packet) else {
        return;
    };
    let Some(search_target) = parse_msearch_target(request) else {
        return;
    };
    let targets = ssdp_targets(&context.udn);
    let matches = if search_target.eq_ignore_ascii_case("ssdp:all") {
        targets
    } else {
        targets
            .into_iter()
            .filter(|target| target.eq_ignore_ascii_case(&search_target))
            .collect()
    };
    if matches.is_empty() {
        return;
    }

    let Some(local_ip) = local_ipv4_for_peer(peer) else {
        return;
    };
    let location = format!(
        "http://{}:{}/description.xml",
        format_ipv4_host(local_ip),
        context.http_port
    );
    for target in matches {
        let response = build_ssdp_search_response(&target, &context.udn, &location);
        let _ = socket.send_to(response.as_bytes(), peer).await;
    }
}

async fn send_ssdp_notify(_socket: &UdpSocket, context: &DmrContext, nts: &str) {
    let destination = SocketAddrV4::new(SSDP_MULTICAST, SSDP_PORT);
    for local_ip in &context.interface_addresses {
        let Ok(sender) = create_ssdp_sender(*local_ip) else {
            continue;
        };
        let location = format!(
            "http://{}:{}/description.xml",
            format_ipv4_host(*local_ip),
            context.http_port
        );
        for target in ssdp_targets(&context.udn) {
            let usn = ssdp_usn(&context.udn, &target);
            let mut message = format!(
                "NOTIFY * HTTP/1.1\r\n\
                 HOST: {SSDP_MULTICAST}:{SSDP_PORT}\r\n\
                 NT: {target}\r\n\
                 NTS: {nts}\r\n\
                 USN: {usn}\r\n\
                 SERVER: {SERVER_HEADER}\r\n"
            );
            if nts.eq_ignore_ascii_case("ssdp:alive") {
                message.push_str(&format!(
                    "CACHE-CONTROL: max-age={SSDP_MAX_AGE_SECONDS}\r\n\
                     LOCATION: {location}\r\n"
                ));
            }
            message.push_str("\r\n");
            let _ = sender.send_to(message.as_bytes(), destination).await;
        }
    }
}

fn create_ssdp_sender(interface: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("无法创建 SSDP 通告套接字")?;
    socket
        .set_multicast_if_v4(&interface)
        .context("无法选择 SSDP 通告网络接口")?;
    socket
        .set_multicast_ttl_v4(2)
        .context("无法设置 SSDP 通告 TTL")?;
    socket
        .bind(&SocketAddrV4::new(interface, 0).into())
        .context("无法绑定 SSDP 通告网络接口")?;
    socket
        .set_nonblocking(true)
        .context("无法将 SSDP 通告套接字设为非阻塞")?;
    let socket: StdUdpSocket = socket.into();
    UdpSocket::from_std(socket).context("无法创建异步 SSDP 通告套接字")
}

fn parse_msearch_target(request: &str) -> Option<String> {
    let mut lines = request.split("\r\n");
    let request_line = lines.next()?.trim();
    if !request_line.eq_ignore_ascii_case("M-SEARCH * HTTP/1.1") {
        return None;
    }

    let mut target = None;
    let mut discover = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("st") {
            target = Some(value.trim().to_owned());
        } else if name.trim().eq_ignore_ascii_case("man")
            && value.to_ascii_lowercase().contains("ssdp:discover")
        {
            discover = true;
        }
    }
    discover
        .then_some(target?)
        .filter(|target| !target.is_empty())
}

fn build_ssdp_search_response(target: &str, udn: &str, location: &str) -> String {
    let usn = ssdp_usn(udn, target);
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={SSDP_MAX_AGE_SECONDS}\r\n\
         EXT:\r\n\
         LOCATION: {location}\r\n\
         SERVER: {SERVER_HEADER}\r\n\
         ST: {target}\r\n\
         USN: {usn}\r\n\
         CONTENT-LENGTH: 0\r\n\
         \r\n"
    )
}

fn ssdp_targets(udn: &str) -> Vec<String> {
    [
        "upnp:rootdevice",
        udn,
        DEVICE_TYPE,
        AV_TRANSPORT_TYPE,
        RENDERING_CONTROL_TYPE,
        CONNECTION_MANAGER_TYPE,
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn ssdp_usn(udn: &str, target: &str) -> String {
    if target.eq_ignore_ascii_case(udn) {
        udn.to_owned()
    } else {
        format!("{udn}::{target}")
    }
}

fn local_ipv4_for_peer(peer: SocketAddr) -> Option<Ipv4Addr> {
    let SocketAddr::V4(peer) = peer else {
        return None;
    };
    let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(peer).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_unspecified() => Some(address),
        _ => None,
    }
}

fn format_ipv4_host(address: Ipv4Addr) -> String {
    address.to_string()
}

async fn run_http_server(
    listener: TcpListener,
    context: Arc<DmrContext>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = accepted.context("DLNA HTTP accept 失败")?;
                let request_context = Arc::clone(&context);
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_http_connection(stream, peer, request_context).await
                    {
                        tracing::debug!("DLNA HTTP {peer} 请求失败：{error:#}");
                    }
                });
            }
        }
    }
}

async fn handle_http_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    context: Arc<DmrContext>,
) -> Result<()> {
    let request = match read_http_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            let response = HttpResponse::plain(400, "Bad Request", format!("Bad Request: {error}"));
            write_http_response(&mut stream, response).await?;
            return Ok(());
        }
    };
    let response = route_http_request(&request, peer, &context);
    write_http_response(&mut stream, response).await
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    let header_end = loop {
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= HTTP_HEADER_LIMIT {
            bail!("HTTP 请求头过大");
        }
        let mut chunk = [0_u8; 8 * 1024];
        let size = stream
            .read(&mut chunk)
            .await
            .context("读取 HTTP 请求失败")?;
        if size == 0 {
            bail!("HTTP 请求在请求头结束前关闭");
        }
        bytes.extend_from_slice(&chunk[..size]);
    };

    let mut parsed_headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut parsed_headers);
    let status = parsed
        .parse(&bytes[..header_end])
        .context("HTTP 请求头格式无效")?;
    if !status.is_complete() {
        bail!("HTTP 请求头不完整");
    }
    let method = parsed.method.context("HTTP 请求缺少方法")?.to_owned();
    let path = parsed
        .path
        .context("HTTP 请求缺少路径")?
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    let headers = parsed
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_ascii_lowercase(),
                String::from_utf8_lossy(header.value).trim().to_owned(),
            )
        })
        .collect::<HashMap<_, _>>();
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        bail!("不支持分块 HTTP 请求");
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .context("Content-Length 无效")?
        .unwrap_or(0);
    if content_length > HTTP_REQUEST_LIMIT {
        bail!("HTTP 请求正文过大");
    }
    let required = header_end
        .checked_add(content_length)
        .context("HTTP 请求长度溢出")?;
    while bytes.len() < required {
        let mut chunk = [0_u8; 8 * 1024];
        let size = stream
            .read(&mut chunk)
            .await
            .context("读取 HTTP 正文失败")?;
        if size == 0 {
            bail!("HTTP 请求正文不完整");
        }
        bytes.extend_from_slice(&chunk[..size]);
        if bytes.len() > HTTP_HEADER_LIMIT + HTTP_REQUEST_LIMIT {
            bail!("HTTP 请求超过限制");
        }
    }

    Ok(HttpRequest {
        method,
        path,
        headers,
        body: bytes[header_end..required].to_vec(),
    })
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn xml(status: u16, reason: &'static str, body: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/xml; charset=utf-8",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }

    fn plain(status: u16, reason: &'static str, body: String) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            headers: Vec::new(),
            body: body.into_bytes(),
        }
    }
}

async fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    let mut header = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Server: {}\r\n\
         Connection: close\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len(),
        SERVER_HEADER
    );
    for (name, value) in response.headers {
        header.push_str(&format!("{name}: {value}\r\n"));
    }
    header.push_str("\r\n");
    stream
        .write_all(header.as_bytes())
        .await
        .context("写入 DLNA HTTP 响应头失败")?;
    stream
        .write_all(&response.body)
        .await
        .context("写入 DLNA HTTP 响应正文失败")?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn route_http_request(
    request: &HttpRequest,
    peer: SocketAddr,
    context: &DmrContext,
) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET" | "HEAD", "/description.xml") => {
            let body = device_description(&context.name, &context.udn);
            response_for_head(request, HttpResponse::xml(200, "OK", body))
        }
        ("GET" | "HEAD", "/upnp/scpd/avtransport.xml") => response_for_head(
            request,
            HttpResponse::xml(200, "OK", AV_TRANSPORT_SCPD.to_owned()),
        ),
        ("GET" | "HEAD", "/upnp/scpd/renderingcontrol.xml") => response_for_head(
            request,
            HttpResponse::xml(200, "OK", RENDERING_CONTROL_SCPD.to_owned()),
        ),
        ("GET" | "HEAD", "/upnp/scpd/connectionmanager.xml") => response_for_head(
            request,
            HttpResponse::xml(200, "OK", CONNECTION_MANAGER_SCPD.to_owned()),
        ),
        ("POST", "/upnp/control/avtransport") => {
            route_soap(request, AV_TRANSPORT_TYPE, |action, arguments| {
                handle_av_transport(&context.controller, peer.ip(), action, arguments)
            })
        }
        ("POST", "/upnp/control/renderingcontrol") => {
            route_soap(request, RENDERING_CONTROL_TYPE, |action, arguments| {
                handle_rendering_control(&context.controller, action, arguments)
            })
        }
        ("POST", "/upnp/control/connectionmanager") => {
            route_soap(request, CONNECTION_MANAGER_TYPE, |action, arguments| {
                handle_connection_manager(&context.controller, action, arguments)
            })
        }
        ("SUBSCRIBE", path) if service_from_event_path(path).is_some() => {
            let service = service_from_event_path(path).expect("guarded service path");
            handle_subscribe(request, peer, context, service)
        }
        ("UNSUBSCRIBE", path) if service_from_event_path(path).is_some() => {
            let service = service_from_event_path(path).expect("guarded service path");
            handle_unsubscribe(request, context, service)
        }
        _ => HttpResponse::plain(404, "Not Found", "Not Found".to_owned()),
    }
}

fn service_from_event_path(path: &str) -> Option<ServiceKind> {
    match path {
        "/upnp/event/avtransport" => Some(ServiceKind::AvTransport),
        "/upnp/event/renderingcontrol" => Some(ServiceKind::RenderingControl),
        "/upnp/event/connectionmanager" => Some(ServiceKind::ConnectionManager),
        _ => None,
    }
}

fn handle_subscribe(
    request: &HttpRequest,
    peer: SocketAddr,
    context: &DmrContext,
    service: ServiceKind,
) -> HttpResponse {
    let initial_body = initial_event_body(service, &context.controller);
    match context.gena.subscribe(service, request, peer, initial_body) {
        Ok((sid, timeout_seconds)) => {
            let mut response = HttpResponse::plain(200, "OK", String::new());
            response.headers.push(("SID".to_owned(), sid));
            response
                .headers
                .push(("TIMEOUT".to_owned(), format!("Second-{timeout_seconds}")));
            response
        }
        Err(message) => HttpResponse::plain(412, "Precondition Failed", message.to_owned()),
    }
}

fn handle_unsubscribe(
    request: &HttpRequest,
    context: &DmrContext,
    service: ServiceKind,
) -> HttpResponse {
    match context.gena.unsubscribe(service, request) {
        Ok(()) => HttpResponse::plain(200, "OK", String::new()),
        Err(message) => HttpResponse::plain(412, "Precondition Failed", message.to_owned()),
    }
}

fn response_for_head(request: &HttpRequest, mut response: HttpResponse) -> HttpResponse {
    if request.method == "HEAD" {
        response.body.clear();
    }
    response
}

fn route_soap(
    request: &HttpRequest,
    service_type: &str,
    handler: impl FnOnce(&str, &HashMap<String, String>) -> SoapResult,
) -> HttpResponse {
    let Some(action) = request
        .headers
        .get("soapaction")
        .and_then(|value| parse_soap_action(value))
    else {
        return soap_fault(401, "Invalid Action");
    };
    let Ok(body) = std::str::from_utf8(&request.body) else {
        return soap_fault(402, "Invalid Args");
    };
    let arguments = soap_arguments(body);
    match handler(&action, &arguments) {
        Ok(values) => HttpResponse::xml(200, "OK", soap_success(service_type, &action, &values)),
        Err(error) => soap_fault(error.code, error.description),
    }
}

fn parse_soap_action(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"');
    let (_, action) = value.rsplit_once('#')?;
    let action = action.trim();
    (!action.is_empty()).then(|| action.to_owned())
}

type SoapResult = std::result::Result<Vec<(&'static str, String)>, SoapError>;

#[derive(Debug)]
struct SoapError {
    code: u16,
    description: &'static str,
}

impl SoapError {
    const fn new(code: u16, description: &'static str) -> Self {
        Self { code, description }
    }
}

fn handle_av_transport(
    controller: &DmrController,
    peer: IpAddr,
    action: &str,
    arguments: &HashMap<String, String>,
) -> SoapResult {
    validate_instance_id(arguments)?;
    match action {
        "SetAVTransportURI" => {
            let uri = required_argument(arguments, "currenturi")?;
            let metadata = argument(arguments, "currenturimetadata").unwrap_or_default();
            controller
                .set_transport_uri_from(uri, metadata, Some(peer))
                .map_err(|_| SoapError::new(716, "Resource not found"))?;
            Ok(Vec::new())
        }
        "SetNextAVTransportURI" => {
            let uri = required_argument(arguments, "nexturi")?;
            let metadata = argument(arguments, "nexturimetadata").unwrap_or_default();
            controller
                .set_next_transport_uri_from(peer, uri, metadata)
                .map_err(|_| SoapError::new(716, "Resource not found"))?;
            Ok(Vec::new())
        }
        "Play" => {
            let speed = argument(arguments, "speed").unwrap_or("1");
            if speed != "1" && speed != "1.0" {
                return Err(SoapError::new(717, "Play speed not supported"));
            }
            controller
                .play_from(peer)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "Pause" => {
            controller
                .pause_from(peer)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "Stop" => {
            controller
                .stop_from(peer)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "Seek" => {
            let unit = required_argument(arguments, "unit")?;
            if !unit.eq_ignore_ascii_case("REL_TIME") && !unit.eq_ignore_ascii_case("ABS_TIME") {
                return Err(SoapError::new(710, "Seek mode not supported"));
            }
            let target = required_argument(arguments, "target")?;
            let position =
                parse_upnp_time(target).ok_or(SoapError::new(711, "Illegal seek target"))?;
            controller
                .seek_from(peer, position)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "GetTransportInfo" => {
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(vec![
                (
                    "CurrentTransportState",
                    snapshot.transport_state.as_upnp().to_owned(),
                ),
                ("CurrentTransportStatus", "OK".to_owned()),
                ("CurrentSpeed", "1".to_owned()),
            ])
        }
        "GetPositionInfo" => {
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            let track = if snapshot.current_uri.is_empty() {
                "0"
            } else {
                "1"
            };
            Ok(vec![
                ("Track", track.to_owned()),
                ("TrackDuration", format_upnp_time(snapshot.duration_ms)),
                ("TrackMetaData", snapshot.metadata.raw),
                ("TrackURI", snapshot.current_uri),
                ("RelTime", format_upnp_time(snapshot.position_ms)),
                ("AbsTime", format_upnp_time(snapshot.position_ms)),
                ("RelCount", "2147483647".to_owned()),
                ("AbsCount", "2147483647".to_owned()),
            ])
        }
        "GetMediaInfo" => {
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            let tracks = if snapshot.current_uri.is_empty() {
                "0"
            } else {
                "1"
            };
            Ok(vec![
                ("NrTracks", tracks.to_owned()),
                ("MediaDuration", format_upnp_time(snapshot.duration_ms)),
                ("CurrentURI", snapshot.current_uri),
                ("CurrentURIMetaData", snapshot.metadata.raw),
                ("NextURI", snapshot.next_uri),
                ("NextURIMetaData", snapshot.next_metadata),
                ("PlayMedium", "NETWORK".to_owned()),
                ("RecordMedium", "NOT_IMPLEMENTED".to_owned()),
                ("WriteStatus", "NOT_IMPLEMENTED".to_owned()),
            ])
        }
        "GetDeviceCapabilities" => Ok(vec![
            ("PlayMedia", "NETWORK".to_owned()),
            ("RecMedia", "NOT_IMPLEMENTED".to_owned()),
            ("RecQualityModes", "NOT_IMPLEMENTED".to_owned()),
        ]),
        "GetTransportSettings" => Ok(vec![
            ("PlayMode", "NORMAL".to_owned()),
            ("RecQualityMode", "NOT_IMPLEMENTED".to_owned()),
        ]),
        "SetPlayMode" => {
            let mode = required_argument(arguments, "newplaymode")?;
            if !mode.eq_ignore_ascii_case("NORMAL") {
                return Err(SoapError::new(712, "Play mode not supported"));
            }
            Ok(Vec::new())
        }
        "Next" => {
            controller
                .next_track_from(peer)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "Previous" => {
            controller
                .previous_track_from(peer)
                .map_err(|_| SoapError::new(701, "Transition not available"))?;
            Ok(Vec::new())
        }
        "GetCurrentTransportActions" => {
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            let actions = current_transport_actions(&snapshot);
            Ok(vec![("Actions", actions.to_owned())])
        }
        _ => Err(SoapError::new(401, "Invalid Action")),
    }
}

fn handle_rendering_control(
    controller: &DmrController,
    action: &str,
    arguments: &HashMap<String, String>,
) -> SoapResult {
    validate_instance_id(arguments)?;
    match action {
        "ListPresets" => Ok(vec![(
            "CurrentPresetNameList",
            "FactoryDefaults".to_owned(),
        )]),
        "SelectPreset" => {
            let preset = required_argument(arguments, "presetname")?;
            if !preset.eq_ignore_ascii_case("FactoryDefaults") {
                return Err(SoapError::new(701, "Invalid Name"));
            }
            Ok(Vec::new())
        }
        "GetMute" => {
            validate_channel(arguments)?;
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(vec![(
                "CurrentMute",
                if snapshot.muted { "1" } else { "0" }.to_owned(),
            )])
        }
        "SetMute" => {
            validate_channel(arguments)?;
            let muted = parse_upnp_bool(required_argument(arguments, "desiredmute")?)
                .ok_or(SoapError::new(402, "Invalid Args"))?;
            controller
                .set_muted(muted)
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(Vec::new())
        }
        "GetVolume" => {
            validate_channel(arguments)?;
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(vec![("CurrentVolume", snapshot.volume.to_string())])
        }
        "SetVolume" => {
            validate_channel(arguments)?;
            let volume = required_argument(arguments, "desiredvolume")?
                .parse::<u8>()
                .map_err(|_| SoapError::new(402, "Invalid Args"))?;
            if volume > 100 {
                return Err(SoapError::new(601, "Argument Value Out of Range"));
            }
            controller
                .set_volume(volume)
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(Vec::new())
        }
        "GetVolumeDB" => {
            validate_channel(arguments)?;
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(vec![(
                "CurrentVolume",
                volume_to_decibels(snapshot.volume).to_string(),
            )])
        }
        "SetVolumeDB" => {
            validate_channel(arguments)?;
            let decibels = required_argument(arguments, "desiredvolume")?
                .parse::<i16>()
                .map_err(|_| SoapError::new(402, "Invalid Args"))?;
            controller
                .set_volume(decibels_to_volume(decibels))
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(Vec::new())
        }
        "GetVolumeDBRange" => {
            validate_channel(arguments)?;
            Ok(vec![
                ("MinValue", "-10240".to_owned()),
                ("MaxValue", "0".to_owned()),
            ])
        }
        _ => Err(SoapError::new(401, "Invalid Action")),
    }
}

fn handle_connection_manager(
    controller: &DmrController,
    action: &str,
    arguments: &HashMap<String, String>,
) -> SoapResult {
    match action {
        "GetProtocolInfo" => Ok(vec![
            ("Source", String::new()),
            ("Sink", SINK_PROTOCOL_INFO.to_owned()),
        ]),
        "GetCurrentConnectionIDs" => {
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            Ok(vec![(
                "ConnectionIDs",
                if snapshot.current_uri.is_empty() {
                    String::new()
                } else {
                    "0".to_owned()
                },
            )])
        }
        "GetCurrentConnectionInfo" => {
            if argument(arguments, "connectionid")
                .unwrap_or("0")
                .parse::<i32>()
                .ok()
                != Some(0)
            {
                return Err(SoapError::new(706, "No Such Connection"));
            }
            let snapshot = controller
                .snapshot()
                .map_err(|_| SoapError::new(501, "Action Failed"))?;
            if snapshot.current_uri.is_empty() {
                return Err(SoapError::new(706, "No Such Connection"));
            }
            Ok(vec![
                ("RcsID", "0".to_owned()),
                ("AVTransportID", "0".to_owned()),
                ("ProtocolInfo", String::new()),
                ("PeerConnectionManager", String::new()),
                ("PeerConnectionID", "-1".to_owned()),
                ("Direction", "Input".to_owned()),
                ("Status", "OK".to_owned()),
            ])
        }
        "ConnectionComplete" => Ok(Vec::new()),
        _ => Err(SoapError::new(401, "Invalid Action")),
    }
}

fn validate_instance_id(arguments: &HashMap<String, String>) -> std::result::Result<(), SoapError> {
    if argument(arguments, "instanceid").unwrap_or("0") == "0" {
        Ok(())
    } else {
        Err(SoapError::new(718, "Invalid InstanceID"))
    }
}

fn validate_channel(arguments: &HashMap<String, String>) -> std::result::Result<(), SoapError> {
    if argument(arguments, "channel")
        .unwrap_or("Master")
        .eq_ignore_ascii_case("Master")
    {
        Ok(())
    } else {
        Err(SoapError::new(402, "Invalid Args"))
    }
}

fn argument<'a>(arguments: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    arguments.get(name).map(String::as_str)
}

fn required_argument<'a>(
    arguments: &'a HashMap<String, String>,
    name: &str,
) -> std::result::Result<&'a str, SoapError> {
    argument(arguments, name).ok_or(SoapError::new(402, "Invalid Args"))
}

fn soap_arguments(body: &str) -> HashMap<String, String> {
    const NAMES: &[&str] = &[
        "InstanceID",
        "CurrentURI",
        "CurrentURIMetaData",
        "NextURI",
        "NextURIMetaData",
        "Speed",
        "Unit",
        "Target",
        "NewPlayMode",
        "PresetName",
        "Channel",
        "DesiredMute",
        "DesiredVolume",
        "ConnectionID",
    ];
    NAMES
        .iter()
        .filter_map(|name| {
            extract_xml_text(body, name).map(|value| (name.to_ascii_lowercase(), value))
        })
        .collect()
}

fn soap_success(service_type: &str, action: &str, values: &[(&str, String)]) -> String {
    let mut arguments = String::new();
    for (name, value) in values {
        arguments.push('<');
        arguments.push_str(name);
        arguments.push('>');
        arguments.push_str(&escape_xml(value));
        arguments.push_str("</");
        arguments.push_str(name);
        arguments.push('>');
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action}Response xmlns:u=\"{service_type}\">{arguments}\
         </u:{action}Response></s:Body></s:Envelope>"
    )
}

fn soap_fault(code: u16, description: &'static str) -> HttpResponse {
    HttpResponse::xml(
        500,
        "Internal Server Error",
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body><s:Fault><faultcode>s:Client</faultcode>\
             <faultstring>UPnPError</faultstring><detail>\
             <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
             <errorCode>{code}</errorCode>\
             <errorDescription>{}</errorDescription>\
             </UPnPError></detail></s:Fault></s:Body></s:Envelope>",
            escape_xml(description)
        ),
    )
}

fn device_description(name: &str, udn: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\" \
         xmlns:dlna=\"urn:schemas-dlna-org:device-1-0\">\
         <specVersion><major>1</major><minor>0</minor></specVersion>\
         <device>\
         <deviceType>{DEVICE_TYPE}</deviceType>\
         <friendlyName>{}</friendlyName>\
         <manufacturer>Microsoft Corporation</manufacturer>\
         <manufacturerURL>https://www.microsoft.com/windows</manufacturerURL>\
         <modelDescription>Windows DLNA Digital Media Renderer</modelDescription>\
         <modelName>Windows Media Receiver</modelName>\
         <modelNumber>0.1</modelNumber>\
         <serialNumber>{}</serialNumber>\
         <UDN>{}</UDN>\
         <dlna:X_DLNADOC>DMR-1.50</dlna:X_DLNADOC>\
         <serviceList>\
         <service><serviceType>{AV_TRANSPORT_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>\
         <SCPDURL>/upnp/scpd/avtransport.xml</SCPDURL>\
         <controlURL>/upnp/control/avtransport</controlURL>\
         <eventSubURL>/upnp/event/avtransport</eventSubURL></service>\
         <service><serviceType>{RENDERING_CONTROL_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>\
         <SCPDURL>/upnp/scpd/renderingcontrol.xml</SCPDURL>\
         <controlURL>/upnp/control/renderingcontrol</controlURL>\
         <eventSubURL>/upnp/event/renderingcontrol</eventSubURL></service>\
         <service><serviceType>{CONNECTION_MANAGER_TYPE}</serviceType>\
         <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
         <SCPDURL>/upnp/scpd/connectionmanager.xml</SCPDURL>\
         <controlURL>/upnp/control/connectionmanager</controlURL>\
         <eventSubURL>/upnp/event/connectionmanager</eventSubURL></service>\
         </serviceList></device></root>",
        escape_xml(name),
        escape_xml(udn.trim_start_matches("uuid:")),
        escape_xml(udn)
    )
}

fn stable_device_uuid(device_key: [u8; 6]) -> String {
    let mut bytes = [
        0x57, 0x69, 0x6e, 0x44, 0x4d, 0x52, 0x40, 0x00, 0x80, 0x00, 0, 0, 0, 0, 0, 0,
    ];
    bytes[10..].copy_from_slice(&device_key);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
         {:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn validate_media_uri(uri: &str) -> Result<()> {
    if uri.is_empty()
        || uri.len() > MEDIA_URI_LIMIT
        || uri.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!("DLNA 媒体地址为空、过长或包含控制字符");
    }
    let scheme = uri.split_once(':').map(|(scheme, _)| scheme).unwrap_or("");
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        bail!("DLNA 仅接受 HTTP/HTTPS 媒体地址");
    }
    Ok(())
}

#[cfg(test)]
fn parse_didl_metadata(metadata: &str) -> MediaMetadata {
    parse_didl_metadata_for_uri(metadata, None)
}

fn parse_didl_metadata_for_uri(metadata: &str, preferred_uri: Option<&str>) -> MediaMetadata {
    if metadata.trim().is_empty() {
        return MediaMetadata::default();
    }
    let resource = select_didl_media_resource(metadata, preferred_uri);
    let resource_tag = resource.as_ref().map(|(tag, _)| tag.as_str());
    let mime_type = resource_tag.and_then(resource_mime_type);
    let duration_ms = resource_tag
        .and_then(|tag| extract_xml_attribute(tag, "duration"))
        .and_then(|value| parse_upnp_time(&value));
    // UPnP DIDL-Lite defines `bitrate` as bytes per second. The JSON bridge
    // exposes bits per second so all source quality displays use one unit.
    // A few control points use non-standard audioBitrate/bitRateBps aliases;
    // those aliases are already bits per second.
    let bitrate_bps = resource_tag
        .and_then(|tag| {
            extract_xml_attribute_alias(
                tag,
                &["bitrate", "audioBitrate", "bitRateBps", "bitrateBps"],
            )
        })
        .and_then(|(name, value)| parse_resource_bitrate_bps(name, &value));
    let sample_rate = resource_tag
        .and_then(|tag| {
            extract_xml_attribute_alias(
                tag,
                &[
                    "sampleFrequency",
                    "sampleRate",
                    "samplingFrequency",
                    "sample_frequency",
                ],
            )
        })
        .and_then(|(_, value)| parse_resource_sample_rate(&value));
    let bits_per_sample = resource_tag
        .and_then(|tag| {
            extract_xml_attribute_alias(tag, &["bitsPerSample", "bitDepth", "bits_per_sample"])
        })
        .and_then(|(_, value)| parse_positive_u16(&value));
    let channels = resource_tag
        .and_then(|tag| {
            extract_xml_attribute_alias(
                tag,
                &[
                    "nrAudioChannels",
                    "audioChannels",
                    "channelCount",
                    "channels",
                    "nr_audio_channels",
                ],
            )
        })
        .and_then(|(_, value)| parse_positive_u16(&value));
    let embedded_lyrics = [
        "synchronizedLyrics",
        "unsynchronizedLyrics",
        "lyrics",
        "lyric",
    ]
    .into_iter()
    .find_map(|name| extract_xml_text(metadata, name))
    .filter(|value| !value.trim().is_empty());
    let explicit_lyrics_uri = ["lyricsURI", "lyricURI", "lyricsUrl", "lyricUrl"]
        .into_iter()
        .find_map(|name| extract_xml_text(metadata, name))
        .filter(|value| !value.trim().is_empty());
    let (lyrics_text, lyrics_uri_from_text) = match embedded_lyrics {
        Some(value) if looks_like_lyrics_uri(&value) => (None, Some(value)),
        value => (value, None),
    };
    let lyrics_uri = explicit_lyrics_uri
        .or(lyrics_uri_from_text)
        .or_else(|| extract_lrc_resource(metadata));

    MediaMetadata {
        title: extract_xml_text(metadata, "title"),
        artist: extract_xml_text(metadata, "artist")
            .or_else(|| extract_xml_text(metadata, "creator")),
        album: extract_xml_text(metadata, "album"),
        artwork_url: extract_xml_text(metadata, "albumArtURI"),
        mime_type,
        bitrate_bps,
        sample_rate,
        bits_per_sample,
        channels,
        upnp_class: extract_xml_text(metadata, "class"),
        duration_ms,
        lyrics_text,
        lyrics_uri,
        raw: metadata.to_owned(),
    }
}

fn select_didl_media_resource(
    metadata: &str,
    preferred_uri: Option<&str>,
) -> Option<(String, String)> {
    let preferred_uri = preferred_uri
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut best: Option<(i32, String, String)> = None;
    for (tag, uri) in extract_xml_elements(metadata, "res") {
        let normalized_uri = uri.trim();
        let mime_type = resource_mime_type(&tag)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut score = 0_i32;
        if preferred_uri.is_some_and(|preferred| normalized_uri == preferred) {
            score += 10_000;
        }
        if mime_type.starts_with("audio/") {
            score += 1_000;
        } else if mime_type.starts_with("video/") {
            score += 900;
        } else if mime_type.starts_with("image/") || mime_type.starts_with("text/") {
            score -= 1_000;
        }
        if looks_like_lrc_path(normalized_uri) {
            score -= 2_000;
        }
        for aliases in [
            &["bitrate", "audioBitrate", "bitRateBps", "bitrateBps"][..],
            &[
                "sampleFrequency",
                "sampleRate",
                "samplingFrequency",
                "sample_frequency",
            ][..],
            &["bitsPerSample", "bitDepth", "bits_per_sample"][..],
            &[
                "nrAudioChannels",
                "audioChannels",
                "channelCount",
                "channels",
                "nr_audio_channels",
            ][..],
        ] {
            if extract_xml_attribute_alias(&tag, aliases).is_some() {
                score += 100;
            }
        }
        if extract_xml_attribute(&tag, "duration").is_some() {
            score += 10;
        }
        if best
            .as_ref()
            .is_none_or(|(best_score, _, _)| score > *best_score)
        {
            best = Some((score, tag, normalized_uri.to_owned()));
        }
    }
    best.map(|(_, tag, uri)| (tag, uri))
        .or_else(|| extract_xml_start_tag(metadata, "res").map(|tag| (tag, String::new())))
}

fn resource_mime_type(tag: &str) -> Option<String> {
    let protocol_info = extract_xml_attribute(tag, "protocolInfo")?;
    let mut fields = protocol_info.splitn(4, ':');
    let _protocol = fields.next();
    let _network = fields.next();
    let declared = fields.next().map(str::trim).unwrap_or_default();
    let additional_info = fields.next().unwrap_or_default();
    if !declared.is_empty() && declared != "*" && !is_generic_mime_type(declared) {
        return Some(declared.to_owned());
    }
    dlna_profile_mime_type(additional_info)
        .map(ToOwned::to_owned)
        .or_else(|| (!declared.is_empty() && declared != "*").then(|| declared.to_owned()))
}

fn is_generic_mime_type(value: &str) -> bool {
    value.eq_ignore_ascii_case("application/octet-stream")
        || value.eq_ignore_ascii_case("application/octetstream")
}

fn dlna_profile_mime_type(additional_info: &str) -> Option<&'static str> {
    let profile = additional_info
        .split(';')
        .find_map(|field| field.trim().strip_prefix("DLNA.ORG_PN="))
        .or_else(|| {
            additional_info
                .split(';')
                .find_map(|field| field.trim().strip_prefix("dlna.org_pn="))
        })?
        .to_ascii_uppercase();
    match profile.as_str() {
        value if value.contains("FLAC") => Some("audio/flac"),
        value if value.contains("LPCM") || value.contains("WAV") => Some("audio/wav"),
        value if value.contains("MP3") => Some("audio/mpeg"),
        value if value.contains("AAC") || value.contains("M4A") => Some("audio/aac"),
        value if value.contains("ALAC") => Some("audio/alac"),
        value if value.contains("OGG") => Some("audio/ogg"),
        value if value.contains("OPUS") => Some("audio/opus"),
        _ => None,
    }
}

fn extract_xml_attribute_alias(
    tag: &str,
    names: &[&'static str],
) -> Option<(&'static str, String)> {
    names
        .iter()
        .find_map(|name| extract_xml_attribute(tag, name).map(|value| (*name, value)))
}

fn parse_decimal_prefix(value: &str) -> Option<f64> {
    let normalized = value.trim().replace(',', "");
    let number = normalized
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();
    let parsed = number.parse::<f64>().ok()?;
    parsed
        .is_finite()
        .then_some(parsed)
        .filter(|value| *value > 0.0)
}

fn parse_resource_bitrate_bps(attribute_name: &str, value: &str) -> Option<u64> {
    let parsed = parse_decimal_prefix(value)?;
    let normalized = value.trim().to_ascii_lowercase();
    let bits_per_second = if normalized.contains("mbps") {
        parsed * 1_000_000.0
    } else if normalized.contains("kbps") || normalized.contains("kbit") {
        parsed * 1_000.0
    } else if normalized.contains("bps") || normalized.contains("bit/s") {
        parsed
    } else if attribute_name.eq_ignore_ascii_case("bitrate") {
        parsed * 8.0
    } else {
        parsed
    };
    positive_f64_to_u64(bits_per_second)
}

fn parse_resource_sample_rate(value: &str) -> Option<u32> {
    let parsed = parse_decimal_prefix(value)?;
    let normalized = value.trim().to_ascii_lowercase();
    let hertz = if normalized.contains("khz") {
        parsed * 1_000.0
    } else {
        parsed
    };
    positive_f64_to_u64(hertz).and_then(|value| u32::try_from(value).ok())
}

fn parse_positive_u16(value: &str) -> Option<u16> {
    parse_decimal_prefix(value)
        .and_then(positive_f64_to_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn positive_f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    if value >= u64::MAX as f64 {
        return Some(u64::MAX);
    }
    Some(value.round() as u64)
}

fn looks_like_lyrics_uri(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.starts_with("http://")
        || normalized.starts_with("https://")
        || normalized.starts_with("file://")
        || looks_like_lrc_path(&normalized)
}

fn looks_like_lrc_path(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.ends_with(".lrc") || normalized.contains(".lrc?") || normalized.contains(".lrc#")
}

fn extract_lrc_resource(metadata: &str) -> Option<String> {
    extract_xml_elements(metadata, "res")
        .into_iter()
        .find_map(|(start_tag, value)| {
            let protocol_info = extract_xml_attribute(&start_tag, "protocolInfo")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let normalized_value = value.trim();
            (!normalized_value.is_empty()
                && (protocol_info.contains("text/lrc")
                    || protocol_info.contains("application/lrc")
                    || looks_like_lrc_path(normalized_value)))
            .then(|| normalized_value.to_owned())
        })
}

fn media_kind(uri: &str, mime_type: Option<&str>, upnp_class: Option<&str>) -> &'static str {
    if let Some(upnp_class) = upnp_class {
        let upnp_class = upnp_class.to_ascii_lowercase();
        if upnp_class.contains("audioitem") {
            return "audio";
        }
        if upnp_class.contains("videoitem") {
            return "video";
        }
        if upnp_class.contains("imageitem") {
            return "image";
        }
    }
    if let Some(mime_type) = mime_type {
        let mime_type = mime_type.to_ascii_lowercase();
        if mime_type.starts_with("audio/") {
            return "audio";
        }
        if mime_type.starts_with("video/") || mime_type.contains("mpegurl") {
            return "video";
        }
        if mime_type.starts_with("image/") {
            return "image";
        }
    }
    let path = uri
        .split(['?', '#'])
        .next()
        .unwrap_or(uri)
        .to_ascii_lowercase();
    if [".mp3", ".m4a", ".aac", ".flac", ".wav", ".wma", ".ogg"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        "audio"
    } else if [
        ".mp4", ".mkv", ".mov", ".avi", ".wmv", ".mpeg", ".mpg", ".m3u8",
    ]
    .iter()
    .any(|extension| path.ends_with(extension))
    {
        "video"
    } else if [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        "image"
    } else {
        "unknown"
    }
}

fn parse_upnp_time(value: &str) -> Option<u64> {
    let mut parts = value.trim().split(':');
    let hours = parts.next()?.parse::<u64>().ok()?;
    let minutes = parts.next()?.parse::<u64>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    if parts.next().is_some()
        || minutes >= 60
        || !(0.0..60.0).contains(&seconds)
        || !seconds.is_finite()
    {
        return None;
    }
    Some(((hours * 3_600 + minutes * 60) as f64 * 1_000.0 + seconds * 1_000.0).round() as u64)
}

fn format_upnp_time(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn parse_upnp_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn volume_to_decibels(volume: u8) -> i16 {
    if volume == 0 {
        -10_240
    } else {
        ((volume as f32 / 100.0).log10() * 5_120.0)
            .round()
            .clamp(-10_240.0, 0.0) as i16
    }
}

fn decibels_to_volume(decibels: i16) -> u8 {
    if decibels <= -10_240 {
        0
    } else {
        (10_f32.powf(decibels.min(0) as f32 / 5_120.0) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8
    }
}

fn extract_xml_text(xml: &str, local_name: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let relative = bytes[cursor..].iter().position(|byte| *byte == b'<')?;
        let start = cursor + relative;
        if start + 1 >= bytes.len() || matches!(bytes[start + 1], b'/' | b'!' | b'?') {
            cursor = start + 1;
            continue;
        }
        let name_start = start + 1;
        let name_end = bytes[name_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
            .map(|offset| name_start + offset)?;
        let qualified_name = &xml[name_start..name_end];
        let candidate = qualified_name.rsplit(':').next().unwrap_or(qualified_name);
        let tag_end_relative = bytes[name_end..].iter().position(|byte| *byte == b'>')?;
        let tag_end = name_end + tag_end_relative;
        if candidate.eq_ignore_ascii_case(local_name) {
            if bytes.get(tag_end.wrapping_sub(1)) == Some(&b'/') {
                return Some(String::new());
            }
            let closing = format!("</{qualified_name}>");
            let content_start = tag_end + 1;
            let closing_start =
                find_ascii_case_insensitive(bytes, closing.as_bytes(), content_start)?;
            let content = xml[content_start..closing_start].trim();
            if let Some(cdata) = content
                .strip_prefix("<![CDATA[")
                .and_then(|value| value.strip_suffix("]]>"))
            {
                return Some(cdata.to_owned());
            }
            return Some(decode_xml_entities(content));
        }
        cursor = tag_end + 1;
    }
    None
}

fn extract_xml_elements(xml: &str, local_name: &str) -> Vec<(String, String)> {
    let bytes = xml.as_bytes();
    let mut cursor = 0;
    let mut elements = Vec::new();
    while cursor < bytes.len() {
        let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'<') else {
            break;
        };
        let start = cursor + relative;
        if start + 1 >= bytes.len() || matches!(bytes[start + 1], b'/' | b'!' | b'?') {
            cursor = start + 1;
            continue;
        }
        let name_start = start + 1;
        let Some(name_end_relative) = bytes[name_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
        else {
            break;
        };
        let name_end = name_start + name_end_relative;
        let qualified_name = &xml[name_start..name_end];
        let candidate = qualified_name.rsplit(':').next().unwrap_or(qualified_name);
        let Some(tag_end) = find_tag_end(bytes, name_end) else {
            break;
        };
        cursor = tag_end + 1;
        if !candidate.eq_ignore_ascii_case(local_name)
            || bytes.get(tag_end.wrapping_sub(1)) == Some(&b'/')
        {
            continue;
        }

        let closing = format!("</{qualified_name}>");
        let content_start = tag_end + 1;
        let Some(closing_start) =
            find_ascii_case_insensitive(bytes, closing.as_bytes(), content_start)
        else {
            continue;
        };
        let content = xml[content_start..closing_start].trim();
        let decoded = content
            .strip_prefix("<![CDATA[")
            .and_then(|value| value.strip_suffix("]]>"))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| decode_xml_entities(content));
        elements.push((xml[start..=tag_end].to_owned(), decoded));
        cursor = closing_start + closing.len();
    }
    elements
}

fn extract_xml_start_tag(xml: &str, local_name: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let relative = bytes[cursor..].iter().position(|byte| *byte == b'<')?;
        let start = cursor + relative;
        if start + 1 >= bytes.len() || matches!(bytes[start + 1], b'/' | b'!' | b'?') {
            cursor = start + 1;
            continue;
        }
        let name_start = start + 1;
        let name_end = bytes[name_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
            .map(|offset| name_start + offset)?;
        let qualified_name = &xml[name_start..name_end];
        let candidate = qualified_name.rsplit(':').next().unwrap_or(qualified_name);
        let tag_end = find_tag_end(bytes, name_end)?;
        if candidate.eq_ignore_ascii_case(local_name) {
            return Some(xml[start..=tag_end].to_owned());
        }
        cursor = tag_end + 1;
    }
    None
}

fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in bytes[start..].iter().copied().enumerate() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(current), value) if current == value => quote = None,
            (None, b'>') => return Some(start + offset),
            _ => {}
        }
    }
    None
}

fn extract_xml_attribute(tag: &str, name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut cursor = 1;
    while cursor < bytes.len() {
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_whitespace() || matches!(bytes[cursor], b'<' | b'/' | b'>'))
        {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/' | b'>')
        {
            cursor += 1;
        }
        if cursor == start {
            break;
        }
        let attribute_name = &tag[start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let quote = *bytes.get(cursor)?;
        if quote != b'\'' && quote != b'"' {
            return None;
        }
        cursor += 1;
        let value_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        let value = tag.get(value_start..cursor)?;
        cursor += 1;
        let local_attribute_name = attribute_name.rsplit(':').next().unwrap_or(attribute_name);
        if attribute_name.eq_ignore_ascii_case(name)
            || local_attribute_name.eq_ignore_ascii_case(name)
        {
            return Some(decode_xml_entities(value));
        }
    }
    None
}

fn decode_xml_entities(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        decoded.push_str(&rest[..index]);
        let after_ampersand = &rest[index + 1..];
        let Some(end) = after_ampersand.find(';') else {
            decoded.push('&');
            rest = after_ampersand;
            continue;
        };
        let entity = &after_ampersand[..end];
        match entity {
            "amp" => decoded.push('&'),
            "lt" => decoded.push('<'),
            "gt" => decoded.push('>'),
            "quot" => decoded.push('"'),
            "apos" => decoded.push('\''),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                if let Ok(code) = u32::from_str_radix(&value[2..], 16)
                    && let Some(character) = char::from_u32(code)
                {
                    decoded.push(character);
                }
            }
            value if value.starts_with('#') => {
                if let Ok(code) = value[1..].parse::<u32>()
                    && let Some(character) = char::from_u32(code)
                {
                    decoded.push(character);
                }
            }
            _ => {
                decoded.push('&');
                decoded.push_str(entity);
                decoded.push(';');
            }
        }
        rest = &after_ampersand[end + 1..];
    }
    decoded.push_str(rest);
    decoded
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|offset| start + offset)
}

const AV_TRANSPORT_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>SetAVTransportURI</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentURI</name><direction>in</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
<argument><name>CurrentURIMetaData</name><direction>in</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetNextAVTransportURI</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>NextURI</name><direction>in</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
<argument><name>NextURIMetaData</name><direction>in</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetMediaInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>NrTracks</name><direction>out</direction><relatedStateVariable>NumberOfTracks</relatedStateVariable></argument>
<argument><name>MediaDuration</name><direction>out</direction><relatedStateVariable>CurrentMediaDuration</relatedStateVariable></argument>
<argument><name>CurrentURI</name><direction>out</direction><relatedStateVariable>AVTransportURI</relatedStateVariable></argument>
<argument><name>CurrentURIMetaData</name><direction>out</direction><relatedStateVariable>AVTransportURIMetaData</relatedStateVariable></argument>
<argument><name>NextURI</name><direction>out</direction><relatedStateVariable>NextAVTransportURI</relatedStateVariable></argument>
<argument><name>NextURIMetaData</name><direction>out</direction><relatedStateVariable>NextAVTransportURIMetaData</relatedStateVariable></argument>
<argument><name>PlayMedium</name><direction>out</direction><relatedStateVariable>PlaybackStorageMedium</relatedStateVariable></argument>
<argument><name>RecordMedium</name><direction>out</direction><relatedStateVariable>RecordStorageMedium</relatedStateVariable></argument>
<argument><name>WriteStatus</name><direction>out</direction><relatedStateVariable>RecordMediumWriteStatus</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetTransportInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentTransportState</name><direction>out</direction><relatedStateVariable>TransportState</relatedStateVariable></argument>
<argument><name>CurrentTransportStatus</name><direction>out</direction><relatedStateVariable>TransportStatus</relatedStateVariable></argument>
<argument><name>CurrentSpeed</name><direction>out</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetPositionInfo</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Track</name><direction>out</direction><relatedStateVariable>CurrentTrack</relatedStateVariable></argument>
<argument><name>TrackDuration</name><direction>out</direction><relatedStateVariable>CurrentTrackDuration</relatedStateVariable></argument>
<argument><name>TrackMetaData</name><direction>out</direction><relatedStateVariable>CurrentTrackMetaData</relatedStateVariable></argument>
<argument><name>TrackURI</name><direction>out</direction><relatedStateVariable>CurrentTrackURI</relatedStateVariable></argument>
<argument><name>RelTime</name><direction>out</direction><relatedStateVariable>RelativeTimePosition</relatedStateVariable></argument>
<argument><name>AbsTime</name><direction>out</direction><relatedStateVariable>AbsoluteTimePosition</relatedStateVariable></argument>
<argument><name>RelCount</name><direction>out</direction><relatedStateVariable>RelativeCounterPosition</relatedStateVariable></argument>
<argument><name>AbsCount</name><direction>out</direction><relatedStateVariable>AbsoluteCounterPosition</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetDeviceCapabilities</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PlayMedia</name><direction>out</direction><relatedStateVariable>PossiblePlaybackStorageMedia</relatedStateVariable></argument>
<argument><name>RecMedia</name><direction>out</direction><relatedStateVariable>PossibleRecordStorageMedia</relatedStateVariable></argument>
<argument><name>RecQualityModes</name><direction>out</direction><relatedStateVariable>PossibleRecordQualityModes</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetTransportSettings</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PlayMode</name><direction>out</direction><relatedStateVariable>CurrentPlayMode</relatedStateVariable></argument>
<argument><name>RecQualityMode</name><direction>out</direction><relatedStateVariable>CurrentRecordQualityMode</relatedStateVariable></argument>
</argumentList></action>
<action><name>Stop</name><argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList></action>
<action><name>Play</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Speed</name><direction>in</direction><relatedStateVariable>TransportPlaySpeed</relatedStateVariable></argument>
</argumentList></action>
<action><name>Pause</name><argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList></action>
<action><name>Seek</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Unit</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekMode</relatedStateVariable></argument>
<argument><name>Target</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SeekTarget</relatedStateVariable></argument>
</argumentList></action>
<action><name>Next</name><argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList></action>
<action><name>Previous</name><argumentList><argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument></argumentList></action>
<action><name>SetPlayMode</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>NewPlayMode</name><direction>in</direction><relatedStateVariable>CurrentPlayMode</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentTransportActions</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Actions</name><direction>out</direction><relatedStateVariable>CurrentTransportActions</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>TransportState</name><dataType>string</dataType><allowedValueList><allowedValue>STOPPED</allowedValue><allowedValue>PLAYING</allowedValue><allowedValue>TRANSITIONING</allowedValue><allowedValue>PAUSED_PLAYBACK</allowedValue><allowedValue>NO_MEDIA_PRESENT</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="yes"><name>TransportStatus</name><dataType>string</dataType><allowedValueList><allowedValue>OK</allowedValue><allowedValue>ERROR_OCCURRED</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>PlaybackStorageMedium</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RecordStorageMedium</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>PossiblePlaybackStorageMedia</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>PossibleRecordStorageMedia</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentPlayMode</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>TransportPlaySpeed</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RecordMediumWriteStatus</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentRecordQualityMode</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>PossibleRecordQualityModes</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NumberOfTracks</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrack</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackDuration</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentMediaDuration</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>AVTransportURI</name><dataType>uri</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>AVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NextAVTransportURI</name><dataType>uri</dataType></stateVariable>
<stateVariable sendEvents="no"><name>NextAVTransportURIMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackMetaData</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTrackURI</name><dataType>uri</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RelativeTimePosition</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AbsoluteTimePosition</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>RelativeCounterPosition</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>AbsoluteCounterPosition</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>CurrentTransportActions</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekMode</name><dataType>string</dataType><allowedValueList><allowedValue>ABS_TIME</allowedValue><allowedValue>REL_TIME</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SeekTarget</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
</serviceStateTable></scpd>"#;

const RENDERING_CONTROL_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>ListPresets</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>CurrentPresetNameList</name><direction>out</direction><relatedStateVariable>PresetNameList</relatedStateVariable></argument>
</argumentList></action>
<action><name>SelectPreset</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>PresetName</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_PresetName</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetMute</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>CurrentMute</name><direction>out</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetMute</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>DesiredMute</name><direction>in</direction><relatedStateVariable>Mute</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetVolume</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetVolume</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>Volume</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetVolumeDB</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>CurrentVolume</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
</argumentList></action>
<action><name>SetVolumeDB</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>DesiredVolume</name><direction>in</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetVolumeDBRange</name><argumentList>
<argument><name>InstanceID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_InstanceID</relatedStateVariable></argument>
<argument><name>Channel</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Channel</relatedStateVariable></argument>
<argument><name>MinValue</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
<argument><name>MaxValue</name><direction>out</direction><relatedStateVariable>VolumeDB</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="yes"><name>LastChange</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>PresetNameList</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_PresetName</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>Mute</name><dataType>boolean</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>Volume</name><dataType>ui2</dataType><allowedValueRange><minimum>0</minimum><maximum>100</maximum><step>1</step></allowedValueRange></stateVariable>
<stateVariable sendEvents="yes"><name>VolumeDB</name><dataType>i2</dataType><allowedValueRange><minimum>-10240</minimum><maximum>0</maximum><step>1</step></allowedValueRange></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Channel</name><dataType>string</dataType><allowedValueList><allowedValue>Master</allowedValue></allowedValueList></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_InstanceID</name><dataType>ui4</dataType></stateVariable>
</serviceStateTable></scpd>"#;

const CONNECTION_MANAGER_SCPD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList>
<action><name>GetProtocolInfo</name><argumentList>
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentConnectionIDs</name><argumentList>
<argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument>
</argumentList></action>
<action><name>GetCurrentConnectionInfo</name><argumentList>
<argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
<argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument>
<argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument>
<argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument>
<argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument>
<argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument>
<argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument>
<argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument>
</argumentList></action>
</actionList>
<serviceStateTable>
<stateVariable sendEvents="yes"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="yes"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable>
</serviceStateTable></scpd>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> DmrController {
        controller_with_events().0
    }

    fn controller_with_events() -> (DmrController, Arc<EventSink>) {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        (
            DmrController::new(Arc::clone(&events), Arc::new(GenaHub::default()), arbiter),
            events,
        )
    }

    fn shared_controller() -> (Arc<DmrController>, Arc<PlaybackArbiter>) {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        let controller = Arc::new(DmrController::new(
            events,
            Arc::new(GenaHub::default()),
            Arc::clone(&arbiter),
        ));
        let weak_controller = Arc::downgrade(&controller);
        arbiter.register_suspender(MediaSource::Dlna, move |lease| {
            if let Some(controller) = weak_controller.upgrade() {
                controller.pause_for_takeover(lease);
            }
        });
        (controller, arbiter)
    }

    fn request(method: &str, path: &str) -> HttpRequest {
        HttpRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn stable_uuid_is_deterministic_and_rfc4122_shaped() {
        let key = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        let first = stable_device_uuid(key);
        let second = stable_device_uuid(key);
        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.as_bytes()[14], b'4');
        assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn ssdp_search_requires_discover_and_returns_requested_target() {
        let request = concat!(
            "M-SEARCH * HTTP/1.1\r\n",
            "HOST: 239.255.255.250:1900\r\n",
            "MAN: \"ssdp:discover\"\r\n",
            "ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n"
        );
        assert_eq!(parse_msearch_target(request).as_deref(), Some(DEVICE_TYPE));
        assert!(parse_msearch_target("NOTIFY * HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn ssdp_response_contains_reachable_location_and_unique_usn() {
        let response = build_ssdp_search_response(
            DEVICE_TYPE,
            "uuid:test-device",
            "http://192.0.2.10:54321/description.xml",
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("LOCATION: http://192.0.2.10:54321/description.xml\r\n"));
        assert!(response.contains(&format!("USN: uuid:test-device::{DEVICE_TYPE}\r\n")));
    }

    #[test]
    fn description_exposes_all_three_dmr_services_and_escapes_name() {
        let description = device_description("LivingRoom-PC & 客厅", "uuid:test");
        assert!(description.contains("<friendlyName>LivingRoom-PC &amp; 客厅</friendlyName>"));
        assert!(description.contains(AV_TRANSPORT_TYPE));
        assert!(description.contains(RENDERING_CONTROL_TYPE));
        assert!(description.contains(CONNECTION_MANAGER_TYPE));
        assert!(description.contains("/upnp/scpd/avtransport.xml"));
        assert!(description.contains("DMR-1.50"));
    }

    #[test]
    fn soap_arguments_decode_uri_and_embedded_didl_lite() {
        let body = r#"<s:Envelope><s:Body><u:SetAVTransportURI>
            <InstanceID>0</InstanceID>
            <CurrentURI>https://example.test/a.mp4?a=1&amp;b=2</CurrentURI>
            <CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;A &amp;amp; B&lt;/dc:title&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>
            </u:SetAVTransportURI></s:Body></s:Envelope>"#;
        let arguments = soap_arguments(body);
        assert_eq!(
            arguments.get("currenturi").map(String::as_str),
            Some("https://example.test/a.mp4?a=1&b=2")
        );
        assert_eq!(
            arguments.get("currenturimetadata").map(String::as_str),
            Some("<DIDL-Lite><item><dc:title>A &amp; B</dc:title></item></DIDL-Lite>")
        );
    }

    #[test]
    fn didl_metadata_preserves_title_artist_album_art_and_duration() {
        let metadata = r#"<DIDL-Lite xmlns:dc="dc" xmlns:upnp="upnp">
            <item><dc:title>Track &amp; Test</dc:title>
            <upnp:artist>Artist</upnp:artist><upnp:album>Album</upnp:album>
            <upnp:class>object.item.audioItem.musicTrack</upnp:class>
            <upnp:albumArtURI>https://example.test/cover.jpg</upnp:albumArtURI>
            <res protocolInfo="http-get:*:audio/flac:*" duration="00:03:05.250"
                 bitrate="176400" sampleFrequency="44100" bitsPerSample="16"
                 nrAudioChannels="2">https://example.test/a.flac</res>
            </item></DIDL-Lite>"#;
        let parsed = parse_didl_metadata(metadata);
        assert_eq!(parsed.title.as_deref(), Some("Track & Test"));
        assert_eq!(parsed.artist.as_deref(), Some("Artist"));
        assert_eq!(parsed.album.as_deref(), Some("Album"));
        assert_eq!(parsed.mime_type.as_deref(), Some("audio/flac"));
        assert_eq!(
            parsed.upnp_class.as_deref(),
            Some("object.item.audioItem.musicTrack")
        );
        assert_eq!(parsed.duration_ms, Some(185_250));
        assert_eq!(parsed.bitrate_bps, Some(1_411_200));
        assert_eq!(parsed.sample_rate, Some(44_100));
        assert_eq!(parsed.bits_per_sample, Some(16));
        assert_eq!(parsed.channels, Some(2));
    }

    #[test]
    fn didl_quality_rejects_zero_or_invalid_values_and_saturates_bitrate() {
        let saturated = parse_didl_metadata(&format!(
            r#"<DIDL-Lite><item><res protocolInfo="http-get:*:audio/flac:*"
                bitrate="{}" sampleFrequency="0" bitsPerSample="nope"
                nrAudioChannels="0">https://example.test/a.flac</res></item></DIDL-Lite>"#,
            u64::MAX,
        ));
        assert_eq!(saturated.bitrate_bps, Some(u64::MAX));
        assert_eq!(saturated.sample_rate, None);
        assert_eq!(saturated.bits_per_sample, None);
        assert_eq!(saturated.channels, None);
    }

    #[test]
    fn didl_quality_selects_the_media_resource_and_accepts_vendor_aliases() {
        let metadata = r#"<DIDL-Lite xmlns:pv="urn:vendor"><item>
            <res protocolInfo="http-get:*:image/jpeg:*">https://example.test/cover.jpg</res>
            <res protocolInfo="http-get:*:application/octet-stream:DLNA.ORG_PN=FLAC"
                 audioBitrate="320 kbps" pv:sampleRate="44.1 kHz"
                 bitDepth="24 bits" channelCount="2 channels">
                 https://example.test/current
            </res>
            <res protocolInfo="http-get:*:text/lrc:*">https://example.test/track.lrc</res>
            </item></DIDL-Lite>"#;

        let parsed = parse_didl_metadata_for_uri(metadata, Some("https://example.test/current"));
        assert_eq!(parsed.mime_type.as_deref(), Some("audio/flac"));
        assert_eq!(parsed.bitrate_bps, Some(320_000));
        assert_eq!(parsed.sample_rate, Some(44_100));
        assert_eq!(parsed.bits_per_sample, Some(24));
        assert_eq!(parsed.channels, Some(2));
    }

    #[test]
    fn active_same_uri_metadata_refresh_republishes_quality_without_losing_position() {
        let (controller, events) = controller_with_events();
        let uri = "https://example.test/late-metadata";
        controller.set_transport_uri(uri, "").unwrap();
        controller.play().unwrap();
        controller.update_playback_state(Some(51_250), Some(180_000), Some(1.0), Some(true));
        let before_refresh = events.captured_events().len();

        controller
            .set_transport_uri(
                uri,
                r#"<DIDL-Lite><item><dc:title>Late metadata</dc:title>
                    <res protocolInfo="http-get:*:audio/flac:*"
                         bitrate="176400" sampleFrequency="96000"
                         bitsPerSample="24" nrAudioChannels="2">
                         https://example.test/late-metadata
                    </res></item></DIDL-Lite>"#,
            )
            .unwrap();

        let refreshed_events = events.captured_events();
        let refreshed_media = refreshed_events[before_refresh..]
            .iter()
            .find(|event| event["type"] == "dlna_media")
            .expect("an active metadata refresh must reach the UI quality projection");
        assert_eq!(refreshed_media["url"], uri);
        assert_eq!(refreshed_media["title"], "Late metadata");
        assert_eq!(refreshed_media["content_type"], "audio/flac");
        assert_eq!(refreshed_media["bitrate_bps"], 1_411_200);
        assert_eq!(refreshed_media["sample_rate"], 96_000);
        assert_eq!(refreshed_media["bits_per_sample"], 24);
        assert_eq!(refreshed_media["channels"], 2);
        assert!(refreshed_media["start_position_ms"].as_u64().unwrap() >= 51_250);
        assert!(controller.snapshot().unwrap().position_ms >= 51_250);
    }

    #[test]
    fn didl_metadata_extracts_embedded_synchronized_lyrics() {
        let metadata = r#"<DIDL-Lite xmlns:dc="dc" xmlns:upnp="upnp">
            <item><dc:title>Track</dc:title>
            <upnp:synchronizedLyrics><![CDATA[[00:01.20]第一行
[00:03.450]第二行]]></upnp:synchronizedLyrics>
            <res protocolInfo="http-get:*:audio/flac:*">https://example.test/a.flac</res>
            </item></DIDL-Lite>"#;

        let parsed = parse_didl_metadata(metadata);
        assert_eq!(
            parsed.lyrics_text.as_deref(),
            Some("[00:01.20]第一行\n[00:03.450]第二行")
        );
        assert_eq!(parsed.lyrics_uri, None);
    }

    #[test]
    fn didl_metadata_finds_explicit_and_lrc_resource_uris() {
        let explicit = r#"<DIDL-Lite xmlns:upnp="upnp"><item>
            <upnp:lyricsURI>lyrics/track.lrc</upnp:lyricsURI>
            <res protocolInfo="http-get:*:audio/mpeg:*">https://example.test/track.mp3</res>
            </item></DIDL-Lite>"#;
        assert_eq!(
            parse_didl_metadata(explicit).lyrics_uri.as_deref(),
            Some("lyrics/track.lrc")
        );

        let resource = r#"<DIDL-Lite><item>
            <res protocolInfo="http-get:*:audio/flac:*">https://example.test/track.flac</res>
            <res protocolInfo="http-get:*:text/lrc:*">https://example.test/track.lrc?token=1</res>
            </item></DIDL-Lite>"#;
        assert_eq!(
            parse_didl_metadata(resource).lyrics_uri.as_deref(),
            Some("https://example.test/track.lrc?token=1")
        );
    }

    #[test]
    fn upnp_class_takes_priority_when_an_audio_hls_uri_has_a_video_like_extension() {
        assert_eq!(
            media_kind(
                "https://example.test/live.m3u8",
                Some("application/vnd.apple.mpegurl"),
                Some("object.item.audioItem.audioBroadcast"),
            ),
            "audio"
        );
    }

    #[test]
    fn transport_actions_update_queryable_state() {
        let controller = controller();
        controller
            .set_transport_uri("https://example.test/movie.mp4", "")
            .unwrap();
        assert_eq!(
            controller.snapshot().unwrap().transport_state,
            TransportState::Stopped
        );
        controller.play().unwrap();
        assert_eq!(
            controller.snapshot().unwrap().transport_state,
            TransportState::Transitioning
        );
        controller.update_playback_state(Some(1_500), Some(20_000), Some(1.0), Some(true));
        assert_eq!(
            controller.snapshot().unwrap().transport_state,
            TransportState::Playing
        );
        controller.pause().unwrap();
        let paused = controller.snapshot().unwrap();
        assert_eq!(paused.transport_state, TransportState::PausedPlayback);
        assert!(paused.position_ms >= 1_500);
        controller.seek(7_500).unwrap();
        assert_eq!(controller.snapshot().unwrap().position_ms, 7_500);
        controller.stop().unwrap();
        assert!(
            controller
                .handle_ui_command("play", None)
                .expect("stopped DLNA URI remains selected")
                .is_ok()
        );
    }

    #[test]
    fn ui_next_and_previous_switch_the_real_dlna_queue() {
        let (controller, events) = controller_with_events();
        let first_metadata = r#"<DIDL-Lite><item><dc:title>First</dc:title>
            <res duration="00:02:00">https://example.test/first.flac</res>
            </item></DIDL-Lite>"#;
        let second_metadata = r#"<DIDL-Lite><item><dc:title>Second</dc:title>
            <res duration="00:03:00">https://example.test/second.flac</res>
            </item></DIDL-Lite>"#;
        controller
            .set_transport_uri("https://example.test/first.flac", first_metadata)
            .unwrap();
        controller
            .set_next_transport_uri("https://example.test/second.flac", second_metadata)
            .unwrap();
        controller.play().unwrap();
        let before_next = events.captured_events().len();

        controller
            .handle_ui_command("next_track", None)
            .expect("DLNA owns the active UI controls")
            .unwrap();
        let next = controller.snapshot().unwrap();
        assert_eq!(next.current_uri, "https://example.test/second.flac");
        assert_eq!(next.previous_uri, "https://example.test/first.flac");
        assert!(next.next_uri.is_empty());
        assert_eq!(next.metadata.title.as_deref(), Some("Second"));
        assert_eq!(next.position_ms, 0);
        assert_eq!(next.transport_state, TransportState::Transitioning);
        assert!(current_transport_actions(&next).contains("Previous"));
        assert!(!current_transport_actions(&next).contains("Next"));
        let next_events = events.captured_events();
        let next_events = &next_events[before_next..];
        let next_media = next_events
            .iter()
            .find(|event| event["type"] == "dlna_media")
            .expect("Next must publish the queued URI to the local decoder");
        assert_eq!(next_media["url"], "https://example.test/second.flac");
        assert_eq!(next_media["title"], "Second");
        assert_eq!(next_media["start_position_ms"], 0);
        let next_capabilities = next_events
            .iter()
            .rev()
            .find(|event| event["type"] == "remote_control_available")
            .expect("Next must refresh the UI capabilities");
        assert_eq!(next_capabilities["source"], "dlna");
        assert!(
            next_capabilities["commands"]
                .as_array()
                .is_some_and(|commands| {
                    commands.iter().any(|command| command == "previous_track")
                        && !commands.iter().any(|command| command == "next_track")
                })
        );
        let before_previous = events.captured_events().len();

        controller
            .handle_ui_command("previous_track", None)
            .expect("DLNA owns the active UI controls")
            .unwrap();
        let previous = controller.snapshot().unwrap();
        assert_eq!(previous.current_uri, "https://example.test/first.flac");
        assert!(previous.previous_uri.is_empty());
        assert_eq!(previous.next_uri, "https://example.test/second.flac");
        assert_eq!(previous.metadata.title.as_deref(), Some("First"));
        assert!(current_transport_actions(&previous).contains("Next"));
        assert!(!current_transport_actions(&previous).contains("Previous"));
        let previous_events = events.captured_events();
        let previous_events = &previous_events[before_previous..];
        let previous_media = previous_events
            .iter()
            .find(|event| event["type"] == "dlna_media")
            .expect("Previous must publish the queued URI to the local decoder");
        assert_eq!(previous_media["url"], "https://example.test/first.flac");
        let previous_capabilities = previous_events
            .iter()
            .rev()
            .find(|event| event["type"] == "remote_control_available")
            .expect("Previous must refresh the UI capabilities");
        assert!(
            previous_capabilities["commands"]
                .as_array()
                .is_some_and(|commands| {
                    commands.iter().any(|command| command == "next_track")
                        && !commands.iter().any(|command| command == "previous_track")
                })
        );
    }

    #[test]
    fn repeated_uri_in_next_queue_resumes_without_resetting_position() {
        let controller = controller();
        let uri = "https://example.test/repeated.flac";
        controller.set_transport_uri(uri, "").unwrap();
        controller.play().unwrap();
        controller.update_playback_state(Some(73_250), Some(180_000), Some(1.0), Some(true));
        controller.pause().unwrap();
        let before = controller.snapshot().unwrap().position_ms;
        controller
            .set_next_transport_uri(
                uri,
                r#"<DIDL-Lite><item><dc:title>Metadata refresh</dc:title>
                    <res duration="00:03:00">https://example.test/repeated.flac</res>
                    </item></DIDL-Lite>"#,
            )
            .unwrap();

        controller
            .handle_ui_command("next_track", None)
            .expect("DLNA owns the active UI controls")
            .unwrap();
        let after = controller.snapshot().unwrap();
        assert_eq!(after.current_uri, uri);
        assert!(after.position_ms >= before);
        assert_eq!(after.metadata.title.as_deref(), Some("Metadata refresh"));
        assert!(after.next_uri.is_empty());
    }

    #[test]
    fn same_uri_metadata_refresh_preserves_resume_position_and_paused_state() {
        let controller = controller();
        let metadata = r#"<DIDL-Lite><item><dc:title>Movie</dc:title>
            <res duration="00:10:00">https://example.test/movie.mp4</res>
            </item></DIDL-Lite>"#;
        let refreshed_metadata = r#"
            <DIDL-Lite>
              <item>
                <dc:title>Movie refreshed</dc:title>
                <upnp:albumArtURI>https://example.test/cover.jpg?token=2&amp;ts=123</upnp:albumArtURI>
                <res duration="00:09:50">https://example.test/movie.mp4</res>
              </item>
            </DIDL-Lite>
        "#;
        controller
            .set_transport_uri("https://example.test/movie.mp4", metadata)
            .unwrap();
        controller.play().unwrap();
        controller.update_playback_state(Some(91_250), Some(600_000), Some(1.0), Some(true));
        controller.pause().unwrap();

        let before = controller.snapshot().unwrap();
        controller
            .set_transport_uri("https://example.test/movie.mp4", refreshed_metadata)
            .unwrap();
        let after = controller.snapshot().unwrap();
        assert_eq!(after.transport_state, TransportState::PausedPlayback);
        assert_eq!(after.position_ms, before.position_ms);
        assert_eq!(after.current_uri, before.current_uri);
        assert_eq!(after.metadata.raw, refreshed_metadata);
        assert_eq!(after.metadata.title.as_deref(), Some("Movie refreshed"));
        assert_eq!(after.duration_ms, 590_000);

        // A metadata-less resource refresh must be idempotent as well.
        controller
            .set_transport_uri("https://example.test/movie.mp4", "")
            .unwrap();
        let after_empty_metadata = controller.snapshot().unwrap();
        assert_eq!(
            after_empty_metadata.transport_state,
            TransportState::PausedPlayback
        );
        assert_eq!(after_empty_metadata.position_ms, before.position_ms);
        assert_eq!(after_empty_metadata.metadata.raw, refreshed_metadata);
    }

    #[test]
    fn same_uri_from_restarted_control_point_keeps_resume_position() {
        let controller = controller();
        let old_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 51));
        let restarted_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 52));
        let uri = "https://example.test/resume-after-reconnect.flac";
        controller
            .set_transport_uri_from(uri, "", Some(old_peer))
            .unwrap();
        controller.play_from(old_peer).unwrap();
        controller.update_playback_state(Some(62_500), Some(200_000), Some(1.0), Some(true));
        controller.pause_from(old_peer).unwrap();
        let before = controller.snapshot().unwrap().position_ms;

        controller
            .set_transport_uri_from(
                uri,
                r#"<DIDL-Lite><item><dc:title>Reconnected</dc:title></item></DIDL-Lite>"#,
                Some(restarted_peer),
            )
            .unwrap();
        let after = controller.snapshot().unwrap();
        assert_eq!(after.position_ms, before);
        assert_eq!(after.transport_state, TransportState::PausedPlayback);
        assert_eq!(after.metadata.title.as_deref(), Some("Reconnected"));
        assert_eq!(
            controller.state.lock().unwrap().owner_peer,
            Some(restarted_peer)
        );
        assert!(controller.play_from(old_peer).is_err());
        assert!(controller.play_from(restarted_peer).is_ok());
    }

    #[test]
    fn selecting_a_different_uri_resets_the_resume_position() {
        let controller = controller();
        controller
            .set_transport_uri("https://example.test/first.mp4", "")
            .unwrap();
        controller.play().unwrap();
        controller.update_playback_state(Some(42_000), Some(120_000), Some(1.0), Some(true));
        controller.pause().unwrap();

        controller
            .set_transport_uri("https://example.test/second.mp4", "")
            .unwrap();
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.transport_state, TransportState::Stopped);
        assert_eq!(snapshot.position_ms, 0);
        assert_eq!(snapshot.current_uri, "https://example.test/second.mp4");
    }

    #[test]
    fn av_transport_description_advertises_supported_time_seek_modes() {
        assert!(AV_TRANSPORT_SCPD.contains("<allowedValue>ABS_TIME</allowedValue>"));
        assert!(AV_TRANSPORT_SCPD.contains("<allowedValue>REL_TIME</allowedValue>"));
    }

    #[test]
    fn invalid_media_schemes_and_seek_times_are_rejected() {
        assert!(validate_media_uri("file:///C:/secret.mp4").is_err());
        assert!(validate_media_uri("https://example.test/a.mp4").is_ok());
        assert_eq!(parse_upnp_time("01:02:03.500"), Some(3_723_500));
        assert_eq!(parse_upnp_time("00:60:00"), None);
        assert_eq!(format_upnp_time(3_723_500), "01:02:03");
        assert!((decibels_to_volume(volume_to_decibels(50)) as i16 - 50).abs() <= 1);
    }

    #[test]
    fn connection_manager_advertises_audio_and_video_sinks() {
        let controller = controller();
        let response =
            handle_connection_manager(&controller, "GetProtocolInfo", &HashMap::new()).unwrap();
        let sink = response
            .into_iter()
            .find(|(name, _)| *name == "Sink")
            .map(|(_, value)| value)
            .unwrap();
        assert!(sink.contains("audio/flac"));
        assert!(sink.contains("video/mp4"));
    }

    #[test]
    fn airplay_takeover_suspends_dlna_and_owner_play_resumes_it() {
        let (controller, arbiter) = shared_controller();
        let owner = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 19));
        let metadata = r#"<DIDL-Lite><item><dc:title>Resume me</dc:title>
            <upnp:artist>Artist</upnp:artist>
            <res duration="00:03:00">https://example.test/movie.mp4</res>
            </item></DIDL-Lite>"#;
        controller
            .set_transport_uri_from("https://example.test/movie.mp4", metadata, Some(owner))
            .unwrap();
        controller
            .set_next_transport_uri_from(
                owner,
                "https://example.test/next.mp4",
                "<DIDL-Lite data-next=\"1\"/>",
            )
            .unwrap();
        assert_eq!(arbiter.current_source(), None);
        controller.play_from(owner).unwrap();
        controller.update_playback_state(Some(47_500), Some(180_000), Some(1.0), Some(true));
        assert!(controller.state.lock().unwrap().renderer_active);

        arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "test_airplay_takeover",
            false,
            |_| (),
        );

        let state = controller.state.lock().unwrap();
        assert!(!state.renderer_active);
        assert_eq!(state.transport_state, TransportState::PausedPlayback);
        assert_eq!(state.rate, 0.0);
        assert!(!state.ready);
        assert!(state.lease.is_none());
        assert_eq!(
            state.current_uri.as_deref(),
            Some("https://example.test/movie.mp4")
        );
        assert_eq!(
            state.next_uri.as_deref(),
            Some("https://example.test/next.mp4")
        );
        assert_eq!(state.next_metadata, "<DIDL-Lite data-next=\"1\"/>");
        assert_eq!(state.owner_peer, Some(owner));
        assert_eq!(state.metadata.raw, metadata);
        assert!(state.position_ms >= 47_500);
        drop(state);
        assert!(controller.handle_ui_command("play", None).is_none());
        assert_eq!(arbiter.current_source(), Some(MediaSource::AirPlayAudio));

        let connection_ids =
            handle_connection_manager(&controller, "GetCurrentConnectionIDs", &HashMap::new())
                .unwrap();
        assert_eq!(connection_ids[0].1, "0");

        controller.play_from(owner).unwrap();
        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));
        let resumed = controller.state.lock().unwrap();
        assert!(resumed.renderer_active);
        assert_eq!(resumed.transport_state, TransportState::Transitioning);
        assert!(resumed.lease.is_some_and(|lease| arbiter.is_current(lease)));
        assert_eq!(
            resumed.current_uri.as_deref(),
            Some("https://example.test/movie.mp4")
        );
        assert!(resumed.position_ms >= 47_500);
    }

    #[test]
    fn selecting_dlna_media_does_not_take_over_until_owner_plays() {
        let (controller, arbiter) = shared_controller();
        let owner = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20));
        arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "test_airplay_takeover",
            false,
            |_| (),
        );

        controller
            .set_transport_uri_from(
                "https://example.test/prepared.mp4",
                "<DIDL-Lite data-prepared=\"1\"/>",
                Some(owner),
            )
            .unwrap();
        assert_eq!(arbiter.current_source(), Some(MediaSource::AirPlayAudio));
        let prepared = controller.state.lock().unwrap();
        assert_eq!(prepared.transport_state, TransportState::Stopped);
        assert!(prepared.lease.is_none());
        assert!(!prepared.renderer_active);
        drop(prepared);

        controller.play_from(owner).unwrap();
        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));
        assert!(controller.state.lock().unwrap().renderer_active);
    }

    #[test]
    fn suspended_owner_can_refresh_metadata_without_losing_resume_position() {
        let (controller, arbiter) = shared_controller();
        let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30));
        let uri = "https://example.test/resume.mp4";
        controller
            .set_transport_uri_from(
                uri,
                "<DIDL-Lite><item><dc:title>Old title</dc:title></item></DIDL-Lite>",
                Some(peer),
            )
            .unwrap();
        controller.play_from(peer).unwrap();
        controller.update_playback_state(Some(81_250), Some(240_000), Some(1.0), Some(true));
        arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "test_airplay_takeover",
            false,
            |_| (),
        );

        controller
            .set_transport_uri_from(
                uri,
                "<DIDL-Lite><item><dc:title>New title</dc:title></item></DIDL-Lite>",
                Some(peer),
            )
            .unwrap();
        assert_eq!(arbiter.current_source(), Some(MediaSource::AirPlayAudio));
        let suspended = controller.snapshot().unwrap();
        assert_eq!(suspended.transport_state, TransportState::PausedPlayback);
        assert!(suspended.position_ms >= 81_250);
        assert_eq!(suspended.metadata.title.as_deref(), Some("New title"));

        controller.play_from(peer).unwrap();
        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));
        assert!(controller.snapshot().unwrap().position_ms >= 81_250);
    }

    #[test]
    fn only_the_selected_control_point_can_mutate_a_dlna_session() {
        let controller = controller();
        let owner = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 40));
        let stale_peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41));
        controller
            .set_transport_uri_from("https://example.test/owned.mp4", "", Some(owner))
            .unwrap();
        controller
            .set_next_transport_uri_from(owner, "https://example.test/owned-next.mp4", "")
            .unwrap();

        assert!(controller.play_from(stale_peer).is_err());
        controller.play_from(owner).unwrap();
        assert!(controller.seek_from(stale_peer, 30_000).is_err());
        assert!(controller.pause_from(stale_peer).is_err());
        assert!(controller.stop_from(stale_peer).is_err());
        assert!(controller.next_track_from(stale_peer).is_err());

        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.transport_state, TransportState::Transitioning);
        assert_eq!(snapshot.position_ms, 0);
        assert_eq!(snapshot.current_uri, "https://example.test/owned.mp4");
    }

    #[test]
    fn sender_seek_before_play_becomes_the_decoder_start_position() {
        let controller = controller();
        controller
            .set_transport_uri("https://example.test/resume.mp4", "")
            .unwrap();
        controller.seek(84_500).unwrap();
        assert_eq!(controller.snapshot().unwrap().position_ms, 84_500);

        controller.play().unwrap();
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.position_ms, 84_500);
        assert_eq!(snapshot.transport_state, TransportState::Transitioning);
    }

    #[test]
    fn soap_next_and_previous_follow_the_selected_control_point_queue() {
        let (controller, events) = controller_with_events();
        let owner = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 60));
        controller
            .set_transport_uri_from(
                "https://example.test/soap-first.flac",
                "<DIDL-Lite><item><dc:title>SOAP first</dc:title></item></DIDL-Lite>",
                Some(owner),
            )
            .unwrap();
        controller
            .set_next_transport_uri_from(
                owner,
                "https://example.test/soap-second.flac",
                "<DIDL-Lite><item><dc:title>SOAP second</dc:title></item></DIDL-Lite>",
            )
            .unwrap();
        controller.play_from(owner).unwrap();
        let arguments = HashMap::from([("instanceid".to_owned(), "0".to_owned())]);
        let before_next = events.captured_events().len();

        handle_av_transport(&controller, owner, "Next", &arguments).unwrap();
        assert_eq!(
            controller.snapshot().unwrap().current_uri,
            "https://example.test/soap-second.flac"
        );
        let next_events = events.captured_events();
        assert!(
            next_events[before_next..].iter().any(|event| {
                event["type"] == "dlna_media"
                    && event["url"] == "https://example.test/soap-second.flac"
                    && event["title"] == "SOAP second"
            }),
            "a successful SOAP Next must publish the new resource to the decoder"
        );

        handle_av_transport(&controller, owner, "Previous", &arguments).unwrap();
        let previous = controller.snapshot().unwrap();
        assert_eq!(previous.current_uri, "https://example.test/soap-first.flac");
        assert_eq!(previous.next_uri, "https://example.test/soap-second.flac");

        let actions =
            handle_av_transport(&controller, owner, "GetCurrentTransportActions", &arguments)
                .unwrap();
        assert_eq!(actions[0].0, "Actions");
        assert!(actions[0].1.contains("Next"));
        assert!(!actions[0].1.contains("Previous"));
    }

    #[test]
    fn soap_route_updates_transport_and_returns_standard_envelope() {
        let gena = Arc::new(GenaHub::default());
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        let controller = Arc::new(DmrController::new(events, Arc::clone(&gena), arbiter));
        let context = DmrContext {
            name: "Windows".to_owned(),
            udn: "uuid:test".to_owned(),
            http_port: 43210,
            interface_addresses: vec![Ipv4Addr::new(192, 0, 2, 10)],
            controller: Arc::clone(&controller),
            gena,
        };
        let mut set_uri = request("POST", "/upnp/control/avtransport");
        set_uri.headers.insert(
            "soapaction".to_owned(),
            format!("\"{AV_TRANSPORT_TYPE}#SetAVTransportURI\""),
        );
        set_uri.body = br#"<s:Envelope><s:Body><u:SetAVTransportURI>
            <InstanceID>0</InstanceID>
            <CurrentURI>https://example.test/movie.mp4</CurrentURI>
            <CurrentURIMetaData></CurrentURIMetaData>
            </u:SetAVTransportURI></s:Body></s:Envelope>"#
            .to_vec();
        let response = route_http_request(
            &set_uri,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345)),
            &context,
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            controller.snapshot().unwrap().current_uri,
            "https://example.test/movie.mp4"
        );
        assert!(
            String::from_utf8(response.body)
                .unwrap()
                .contains("SetAVTransportURIResponse")
        );
    }

    #[test]
    fn callback_must_be_http_and_match_the_subscribing_peer_ip() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 25), 50000));
        let callback = parse_callback_url("<http://192.0.2.25:1400/events>", peer).unwrap();
        assert_eq!(callback.port, 1400);
        assert_eq!(callback.path_and_query, "/events");
        assert!(parse_callback_url("<http://192.0.2.26:1400/events>", peer).is_err());
        assert!(parse_callback_url("<http://renderer.test:1400/events>", peer).is_err());
        assert!(parse_callback_url("<https://192.0.2.25/events>", peer).is_err());
        assert!(parse_callback_url("<http://user@192.0.2.25/events>", peer).is_err());
    }

    #[tokio::test]
    async fn gena_subscription_sends_initial_notify_and_can_be_cancelled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let callback_port = listener.local_addr().unwrap().port();
        let hub = Arc::new(GenaHub::default());
        let mut subscribe = request("SUBSCRIBE", "/upnp/event/avtransport");
        subscribe
            .headers
            .insert("nt".to_owned(), "upnp:event".to_owned());
        subscribe.headers.insert(
            "callback".to_owned(),
            format!("<http://127.0.0.1:{callback_port}/events>"),
        );
        subscribe
            .headers
            .insert("timeout".to_owned(), "Second-300".to_owned());
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 55000));
        let initial_body = last_change_propertyset("<Event/>");
        let (sid, timeout_seconds) = hub
            .subscribe(ServiceKind::AvTransport, &subscribe, peer, initial_body)
            .unwrap();
        assert_eq!(timeout_seconds, 300);

        let (mut callback, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = [0_u8; 4_096];
        let size = callback.read(&mut bytes).await.unwrap();
        let notification = String::from_utf8_lossy(&bytes[..size]);
        assert!(notification.starts_with("NOTIFY /events HTTP/1.1\r\n"));
        assert!(notification.contains(&format!("SID: {sid}\r\n")));
        assert!(notification.contains("SEQ: 0\r\n"));
        assert!(notification.contains("<LastChange>"));
        callback
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();

        let mut unsubscribe = request("UNSUBSCRIBE", "/upnp/event/avtransport");
        unsubscribe.headers.insert("sid".to_owned(), sid.clone());
        hub.unsubscribe(ServiceKind::AvTransport, &unsubscribe)
            .unwrap();
        assert!(!hub.subscriptions.lock().unwrap().contains_key(&sid));
    }
}
