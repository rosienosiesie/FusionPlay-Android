use aes::Aes128;
use aes::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, block_padding::NoPadding};
use anyhow::{Context, Result, bail};
use cbc::{Decryptor, Encryptor};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::MiPlayDeviceType;
use crate::media::{EventEmitter, PlaybackGate, StreamKeys, spawn_rtsp_receiver};

pub(crate) const CONTROL_PORT: u16 = 8899;
const PROTOCOL_VERSION: &str = "2.1.5071614";

const GET_VERSION: u8 = 0x36;
const GET_VERSION_ACK: u8 = 0x37;
const DEVICE_ID: u8 = 0x28;
const AUTH_ACK: u8 = 0x29;
const SAFETY_INFO: u8 = 0x00;
const SAFETY_INFO_ACK: u8 = 0x01;
const SAFETY_AUTH: u8 = 0x02;
const SAFETY_AUTH_ACK: u8 = 0x03;
const GET_DEVICE_INFO: u8 = 0x1e;
const GET_DEVICE_INFO_ACK: u8 = 0x1f;
const SET_DEVICE_INFO: u8 = 0x58;
const SET_DEVICE_INFO_ACK: u8 = 0x59;
const GET_MIRROR_MODE: u8 = 0x34;
const GET_MIRROR_MODE_ACK: u8 = 0x35;
const SET_MIRROR_KEY: u8 = 0x6c;
const SET_MIRROR_KEY_ACK: u8 = 0x6d;
const OPEN: u8 = 0x00;
const OPEN_ACK: u8 = 0x01;
const PAUSE: u8 = 0x04;
const PAUSE_ACK: u8 = 0x05;
const RESUME: u8 = 0x06;
const RESUME_ACK: u8 = 0x07;
const SET_VOLUME: u8 = 0x0c;
const SET_VOLUME_ACK: u8 = 0x0d;
const GET_VOLUME: u8 = 0x0e;
const GET_VOLUME_ACK: u8 = 0x0f;
const SET_MEDIA_INFO: u8 = 0x12;
const SET_MEDIA_INFO_ACK: u8 = 0x13;
const GET_MEDIA_INFO: u8 = 0x14;
const HEART_BEAT: u8 = 0x1a;
const HEART_BEAT_ACK: u8 = 0x1b;
const GET_STATE: u8 = 0x1c;
const GET_STATE_ACK: u8 = 0x1d;
const NOTIFY: u8 = 0x22;
const SET_PLAY_SOURCE: u8 = 0x40;
const SET_PLAY_SOURCE_ACK: u8 = 0x41;
const PAUSE_MEDIA_PLAYER_ACK: u8 = 0x45;
const RESUME_MEDIA_PLAYER_ACK: u8 = 0x47;
const SEEK_MEDIA_PLAYER_ACK: u8 = 0x49;
const PREVIOUS_MEDIA_PLAYER_ACK: u8 = 0x4b;
const NEXT_MEDIA_PLAYER_ACK: u8 = 0x4d;
const SET_POSITION: u8 = 0x56;
const SET_POSITION_ACK: u8 = 0x57;
const SET_MEDIA_STATE: u8 = 0x5e;
const SET_MEDIA_STATE_ACK: u8 = 0x5f;
const MOBILE_AUDIO_STREAMING_MODE_ACK: [u8; 5] = [0, 0, 0, 0, 1];
// MiPCAudio uses one monotonically increasing sequence space for unsolicited
// receiver-originated frames. Captured HyperOS sessions start with
// mode/mediaInfoEx/state at 5..7. FusionPlay publishes media notifications and
// receiver controls from 8 onward. Reusing a fixed sequence makes later
// pause/previous/next/seek frames look like duplicates and they are silently
// discarded by CmdSource.
const FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE: u32 = 8;

// Xiaomi encodes the standalone-player command family in the high byte of
// the 16-bit command id. Keeping the namespace separate is important: for
// example 0x0416 is not the ordinary low-byte command 0x16.
const ALONE_MEDIA_PLAYER_NAMESPACE: u8 = 0x04;
const ALONE_MEDIA_PLAYER_SET_STATE: u8 = 0x14;
const ALONE_MEDIA_PLAYER_SET_STATE_ACK: u8 = 0x15;
const ALONE_MEDIA_PLAYER_GET_STATE: u8 = 0x16;
const ALONE_MEDIA_PLAYER_GET_STATE_ACK: u8 = 0x17;
const ALONE_MEDIA_PLAYER_SET_MEDIA_INFO: u8 = 0x18;
const ALONE_MEDIA_PLAYER_SET_MEDIA_INFO_ACK: u8 = 0x19;

#[derive(Clone, Copy, Debug)]
pub enum MediaAction {
    Toggle,
    Pause,
    Resume,
    Previous,
    Next,
    Seek(u64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaControlOutcome {
    pub dispatched: bool,
    pub confirmed: bool,
}

/// Observable stages of the HyperConnect-compatible receiver path.
///
/// A transport connection and the legacy SafetyAuth exchange are deliberately
/// not treated as a playable route.  Xiaomi's public HyperOS architecture puts
/// device trust and capability negotiation between discovery and the media
/// session, so callers must wait for `MediaSessionEstablished` before exposing
/// playback controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterconnectStage {
    TransportConnected,
    SecureChannelEstablished,
    IdentityExchanged,
    CapabilitiesExchanged,
    MediaSessionEstablished,
    TransportDisconnected,
}

impl InterconnectStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TransportConnected => "transport_connected",
            Self::SecureChannelEstablished => "secure_channel_established",
            Self::IdentityExchanged => "identity_exchanged",
            Self::CapabilitiesExchanged => "capabilities_exchanged",
            Self::MediaSessionEstablished => "media_session_established",
            Self::TransportDisconnected => "transport_disconnected",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub device_name: String,
    pub model: String,
    pub platform: String,
    pub device_type: MiPlayDeviceType,
}

#[derive(Clone)]
pub struct ControlHub {
    active: Arc<Mutex<Option<ActiveSession>>>,
    sessions: Arc<Mutex<HashMap<u64, TcpStream>>>,
    control_routes: Arc<Mutex<HashMap<u64, ControlRoute>>>,
    stream_keys: Arc<Mutex<HashMap<IpAddr, SharedStreamKeys>>>,
    media_generation: Arc<AtomicU64>,
    next_effect_id: Arc<AtomicU64>,
    remote_effects: Arc<Mutex<RemoteEffectState>>,
    control_dispatch: Arc<Mutex<ControlDispatchState>>,
    volume_percent: Arc<AtomicU32>,
    output_suspended: Arc<AtomicBool>,
}

#[derive(Default)]
struct ControlDispatchState {
    recent_playback: Option<RecentPlaybackDispatch>,
}

struct RecentPlaybackDispatch {
    session_id: u64,
    paused: bool,
    sent_at: Instant,
}

#[derive(Clone)]
struct ControlRoute {
    remote_ip: IpAddr,
    sender: Sender<Outgoing>,
    next_notification_sequence: Arc<AtomicU32>,
    reverse_control_ready: bool,
    play_source_registered: bool,
    updated_at: Instant,
}

#[derive(Clone)]
struct ActiveSession {
    id: u64,
    remote_ip: IpAddr,
    sender: Sender<Outgoing>,
    playback_gate: PlaybackGate,
    next_notification_sequence: Arc<AtomicU32>,
}

#[derive(Default)]
struct RemoteEffectState {
    track_revision: u64,
    position_revision: u64,
    track_identity: Option<String>,
    last_position: Option<(u64, u64)>,
    pending: HashMap<u64, PendingRemoteEffect>,
}

struct PendingRemoteEffect {
    session_id: u64,
    expected: ExpectedRemoteEffect,
    completion: Sender<bool>,
}

enum ExpectedRemoteEffect {
    Playback {
        paused: bool,
    },
    TrackChange {
        baseline_revision: u64,
    },
    Position {
        target_ms: u64,
        baseline_revision: u64,
    },
}

#[derive(Clone)]
struct SharedStreamKeys {
    keys: StreamKeys,
    session_id: u64,
}

impl ControlHub {
    fn new(volume_percent: Arc<AtomicU32>) -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            control_routes: Arc::new(Mutex::new(HashMap::new())),
            stream_keys: Arc::new(Mutex::new(HashMap::new())),
            media_generation: Arc::new(AtomicU64::new(0)),
            next_effect_id: Arc::new(AtomicU64::new(1)),
            remote_effects: Arc::new(Mutex::new(RemoteEffectState::default())),
            control_dispatch: Arc::new(Mutex::new(ControlDispatchState::default())),
            volume_percent,
            output_suspended: Arc::new(AtomicBool::new(false)),
        }
    }

    fn register_session(&self, id: u64, stream: &TcpStream) -> Result<()> {
        let shutdown_stream = stream
            .try_clone()
            .context("clone MiPlay control socket for shutdown")?;
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay control session registry is poisoned"))?
            .insert(id, shutdown_stream);
        Ok(())
    }

    fn register_control_route(
        &self,
        id: u64,
        remote_ip: IpAddr,
        sender: Sender<Outgoing>,
        next_notification_sequence: Arc<AtomicU32>,
    ) {
        if let Ok(mut routes) = self.control_routes.lock() {
            routes.insert(
                id,
                ControlRoute {
                    remote_ip,
                    sender,
                    next_notification_sequence,
                    reverse_control_ready: false,
                    play_source_registered: false,
                    updated_at: Instant::now(),
                },
            );
        }
    }

    fn mark_play_source_registered(&self, id: u64) {
        if let Ok(mut routes) = self.control_routes.lock()
            && let Some(route) = routes.get_mut(&id)
        {
            route.play_source_registered = true;
            route.updated_at = Instant::now();
        }
    }

    fn mark_reverse_control_ready(&self, id: u64) {
        if let Ok(mut routes) = self.control_routes.lock()
            && let Some(route) = routes.get_mut(&id)
        {
            route.reverse_control_ready = true;
            route.updated_at = Instant::now();
        }
    }

    fn preferred_control_route(&self, active: &ActiveSession) -> Option<ControlRoute> {
        self.control_routes.lock().ok().and_then(|routes| {
            routes
                .iter()
                .filter(|(_, route)| route.remote_ip == active.remote_ip)
                .max_by_key(|(id, route)| {
                    (
                        route.reverse_control_ready,
                        route.play_source_registered,
                        route.updated_at,
                        **id == active.id,
                    )
                })
                .map(|(_, route)| route.clone())
        })
    }

    pub(crate) fn shutdown_sessions(&self) {
        self.media_generation.fetch_add(1, Ordering::AcqRel);
        let sessions = self
            .sessions
            .lock()
            .ok()
            .map(|mut sessions| {
                sessions
                    .drain()
                    .map(|(_, stream)| stream)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for stream in sessions {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Ok(mut routes) = self.control_routes.lock() {
            routes.clear();
        }
    }

    pub fn send_confirmed(
        &self,
        action: MediaAction,
        timeout: Duration,
    ) -> Result<MediaControlOutcome> {
        // Keep action resolution, duplicate detection and queueing atomic. JNI
        // calls can arrive concurrently when the UI receives a rapid click
        // burst, while the first caller is still waiting for the phone's state
        // confirmation.
        let mut dispatch = self
            .control_dispatch
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay control-dispatch lock poisoned"))?;
        let resolved_action = match action {
            MediaAction::Toggle if self.is_paused() => MediaAction::Resume,
            MediaAction::Toggle => MediaAction::Pause,
            action => action,
        };
        // MiPCAudio's receiver UI controls the source-side MediaSession through
        // ServerApp::notifyCtrlCommand. Direct MediaPlayer commands (0x44..0x4c)
        // belong to the opposite direction and only affect the receiver/local
        // state when sent here. HyperOS consumes these NOTIFY keys in
        // CmdSource::onRecvNotify and forwards them to the phone MediaSession.
        let body = match resolved_action {
            MediaAction::Toggle => unreachable!("toggle is resolved above"),
            MediaAction::Pause => receiver_control_boolean(b"key-pause"),
            MediaAction::Resume => receiver_control_boolean(b"key-resume"),
            MediaAction::Previous => receiver_control_boolean(b"key-prev"),
            MediaAction::Next => receiver_control_boolean(b"key-next"),
            MediaAction::Seek(position_ms) => receiver_control_u64(b"key-seek", position_ms),
        };
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay control lock poisoned"))?
            .clone()
            .context("no active Xiaomi MiPlay source")?;
        let control_route = self.preferred_control_route(&active);
        let control_sender = control_route
            .as_ref()
            .map(|route| &route.sender)
            .unwrap_or(&active.sender);
        let notification_sequence = control_route
            .as_ref()
            .map(|route| &route.next_notification_sequence)
            .unwrap_or(&active.next_notification_sequence);
        let expected_paused = match resolved_action {
            MediaAction::Pause => Some(true),
            MediaAction::Resume => Some(false),
            _ => None,
        };
        if !matches!(action, MediaAction::Toggle)
            && expected_paused
                .is_some_and(|paused| active.playback_gate.is_source_paused() == paused)
        {
            return Ok(MediaControlOutcome {
                dispatched: false,
                confirmed: true,
            });
        }
        if let Some(paused) = expected_paused
            && dispatch.recent_playback.as_ref().is_some_and(|recent| {
                recent.session_id == active.id
                    && recent.paused == paused
                    && recent.sent_at.elapsed() < Duration::from_millis(750)
            })
        {
            // The matching frame is already on the transport. Report it as
            // dispatched so callers do not surface a false failure, but keep
            // confirmed=false until the phone publishes the authoritative
            // playback state.
            return Ok(MediaControlOutcome {
                dispatched: true,
                confirmed: false,
            });
        }
        let effect_id = self.next_effect_id.fetch_add(1, Ordering::Relaxed);
        let (completion, confirmation) = mpsc::channel();
        {
            let mut effects = self
                .remote_effects
                .lock()
                .map_err(|_| anyhow::anyhow!("MiPlay remote-effect lock poisoned"))?;
            let expected = match resolved_action {
                MediaAction::Toggle => unreachable!("toggle is resolved above"),
                MediaAction::Pause => ExpectedRemoteEffect::Playback { paused: true },
                MediaAction::Resume => ExpectedRemoteEffect::Playback { paused: false },
                MediaAction::Previous | MediaAction::Next => ExpectedRemoteEffect::TrackChange {
                    baseline_revision: effects.track_revision,
                },
                MediaAction::Seek(target_ms) => ExpectedRemoteEffect::Position {
                    target_ms,
                    baseline_revision: effects.position_revision,
                },
            };
            effects.pending.insert(
                effect_id,
                PendingRemoteEffect {
                    session_id: active.id,
                    expected,
                    completion,
                },
            );
        }

        let sequence = allocate_notification_sequence(notification_sequence);
        if let Err(error) = control_sender.send(Outgoing::encrypted(NOTIFY, sequence, body)) {
            self.remove_pending_effect(effect_id);
            return Err(error).context("send Xiaomi MiPlay receiver-control notification");
        }

        if let Some(paused) = expected_paused {
            // Some HyperOS senders apply a reverse-control key without echoing
            // a PAUSE/RESUME frame. Project the successfully queued command so
            // the next toggle resolves in the opposite direction instead of
            // becoming permanently stuck on the same key. Any later source
            // state or advancing position still reconciles this value after
            // the pause settling window has excluded queued stale progress.
            active.playback_gate.set_paused(paused);
            dispatch.recent_playback = Some(RecentPlaybackDispatch {
                session_id: active.id,
                paused,
                sent_at: Instant::now(),
            });
        }
        drop(dispatch);

        let confirmed = confirmation.recv_timeout(timeout).unwrap_or(false);
        self.remove_pending_effect(effect_id);
        Ok(MediaControlOutcome {
            dispatched: true,
            confirmed,
        })
    }

    pub fn send(&self, action: MediaAction) -> Result<()> {
        self.send_confirmed(action, Duration::from_secs(3))?;
        Ok(())
    }

    /// Publishes a receiver-originated volume change to the active MiPlay
    /// source. HyperOS consumes the same type-7 `volume` notification that is
    /// used to confirm a sender-originated SET_VOLUME command.
    pub fn set_volume(&self, percent: u8) -> Result<()> {
        let percent = u32::from(percent.min(100));
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay control lock poisoned"))?
            .clone()
            .context("no active Xiaomi MiPlay source")?;
        let control_route = self.preferred_control_route(&active);
        let control_sender = control_route
            .as_ref()
            .map(|route| &route.sender)
            .unwrap_or(&active.sender);
        let notification_sequence = control_route
            .as_ref()
            .map(|route| &route.next_notification_sequence)
            .unwrap_or(&active.next_notification_sequence);
        let sequence = allocate_notification_sequence(notification_sequence);
        control_sender
            .send(Outgoing::encrypted(
                NOTIFY,
                sequence,
                volume_notification(percent),
            ))
            .context("send Xiaomi MiPlay receiver-volume notification")?;
        self.volume_percent.store(percent, Ordering::Release);
        Ok(())
    }

    fn activate(
        &self,
        id: u64,
        remote_ip: IpAddr,
        sender: Sender<Outgoing>,
        next_notification_sequence: Arc<AtomicU32>,
    ) -> PlaybackGate {
        let Ok(mut active) = self.active.lock() else {
            return PlaybackGate::with_output_suspension(Arc::clone(&self.output_suspended));
        };
        // Construct and publish the gate while holding the same lock used by
        // suspend_output/resume_output. This closes the race where a gate was
        // initialized from the old ownership flag and installed only after a
        // competing source had already changed that flag.
        let playback_gate =
            PlaybackGate::with_output_suspension(Arc::clone(&self.output_suspended));
        *active = Some(ActiveSession {
            id,
            remote_ip,
            sender,
            playback_gate: playback_gate.clone(),
            next_notification_sequence,
        });
        drop(active);
        if let Ok(mut effects) = self.remote_effects.lock() {
            effects.track_identity = None;
            effects.last_position = None;
        }
        playback_gate
    }

    fn remove_pending_effect(&self, effect_id: u64) {
        if let Ok(mut effects) = self.remote_effects.lock() {
            effects.pending.remove(&effect_id);
        }
    }

    fn observe_playback(&self, id: u64, paused: bool) -> bool {
        let target = self.active.lock().ok().and_then(|active| {
            active
                .as_ref()
                .filter(|session| session.id == id)
                .map(|session| session.playback_gate.clone())
        });
        let Some(target) = target else {
            return false;
        };
        target.set_paused(paused);
        self.complete_matching_effects(id, |expected| {
            matches!(expected, ExpectedRemoteEffect::Playback { paused: value } if *value == paused)
        });
        true
    }

    fn observe_playback_snapshot(&self, id: u64, paused: bool) -> bool {
        let target = self.active.lock().ok().and_then(|active| {
            active
                .as_ref()
                .filter(|session| session.id == id)
                .map(|session| session.playback_gate.clone())
        });
        let Some(target) = target else {
            return false;
        };
        // HyperOS replays cached SET_MEDIA_STATE=paused frames while restoring
        // a route, even though position updates and RTP audio keep advancing.
        // Snapshot messages are consequently safe as a resume hint but not as
        // local mute authority. Real pauses still arrive as the explicit
        // PAUSE command or an RTSP PAUSE request and use observe_playback.
        if paused {
            return false;
        }
        if !target.accepts_weak_resume() {
            return false;
        }
        self.complete_matching_effects(id, |expected| {
            matches!(expected, ExpectedRemoteEffect::Playback { paused: false })
        });
        true
    }

    fn observe_media_info(&self, id: u64, body: &[u8]) {
        let identity = media_identity(body);
        let completions = {
            let Ok(mut effects) = self.remote_effects.lock() else {
                return;
            };
            let changed = identity
                .as_ref()
                .is_some_and(|identity| effects.track_identity.as_ref() != Some(identity));
            if changed {
                effects.track_identity = identity;
                effects.track_revision = effects.track_revision.wrapping_add(1);
                effects.last_position = None;
            }
            let revision = effects.track_revision;
            drain_matching_effects(&mut effects, id, |expected| {
                matches!(
                    expected,
                    ExpectedRemoteEffect::TrackChange { baseline_revision }
                        if revision > *baseline_revision
                )
            })
        };
        complete_remote_effects(completions, true);
    }

    fn observe_position(&self, id: u64, position_ms: u64) -> (bool, bool) {
        let (completions, changed, advancing) = {
            let Ok(mut effects) = self.remote_effects.lock() else {
                return (false, false);
            };
            let advancing = effects
                .last_position
                .is_some_and(|(session_id, previous_ms)| {
                    session_id == id && position_ms > previous_ms.saturating_add(250)
                });
            let changed = effects.last_position != Some((id, position_ms));
            effects.last_position = Some((id, position_ms));
            effects.position_revision = effects.position_revision.wrapping_add(1);
            let revision = effects.position_revision;
            (
                drain_matching_effects(&mut effects, id, |expected| {
                    matches!(
                        expected,
                        ExpectedRemoteEffect::Position { target_ms, baseline_revision }
                            if revision > *baseline_revision
                                && position_ms.abs_diff(*target_ms) <= 2_000
                    )
                }),
                changed,
                advancing,
            )
        };
        complete_remote_effects(completions, true);
        let resumed = advancing && self.resume_if_paused(id);
        (changed, resumed)
    }

    fn resume_if_paused(&self, id: u64) -> bool {
        let target = self.active.lock().ok().and_then(|active| {
            active
                .as_ref()
                .filter(|session| session.id == id)
                .map(|session| session.playback_gate.clone())
        });
        let Some(target) = target else {
            return false;
        };
        if !target.is_paused() || !target.accepts_weak_resume() {
            return false;
        }
        self.complete_matching_effects(id, |expected| {
            matches!(expected, ExpectedRemoteEffect::Playback { paused: false })
        });
        true
    }

    fn complete_matching_effects(
        &self,
        id: u64,
        predicate: impl Fn(&ExpectedRemoteEffect) -> bool,
    ) {
        let completions = {
            let Ok(mut effects) = self.remote_effects.lock() else {
                return;
            };
            drain_matching_effects(&mut effects, id, predicate)
        };
        complete_remote_effects(completions, true);
    }

    fn deactivate(&self, id: u64) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&id);
        }
        if let Ok(mut routes) = self.control_routes.lock() {
            routes.remove(&id);
        }
        if let Ok(mut active) = self.active.lock()
            && active.as_ref().is_some_and(|current| current.id == id)
        {
            *active = None;
        }
        let completions = self
            .remote_effects
            .lock()
            .ok()
            .map(|mut effects| {
                let matching: Vec<u64> = effects
                    .pending
                    .iter()
                    .filter_map(|(effect_id, pending)| {
                        (pending.session_id == id).then_some(*effect_id)
                    })
                    .collect();
                matching
                    .into_iter()
                    .filter_map(|effect_id| effects.pending.remove(&effect_id))
                    .map(|pending| pending.completion)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Ok(mut effects) = self.remote_effects.lock()
            && effects
                .last_position
                .is_some_and(|(session_id, _)| session_id == id)
        {
            effects.last_position = None;
        }
        complete_remote_effects(completions, false);
    }

    fn remember_stream_keys(
        &self,
        remote: IpAddr,
        session_id: u64,
        keys: StreamKeys,
    ) -> Result<()> {
        self.stream_keys
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay stream-key lock poisoned"))?
            .insert(remote, SharedStreamKeys { keys, session_id });
        Ok(())
    }

    fn stream_keys_for(&self, remote: IpAddr) -> Result<Option<SharedStreamKeys>> {
        Ok(self
            .stream_keys
            .lock()
            .map_err(|_| anyhow::anyhow!("MiPlay stream-key lock poisoned"))?
            .get(&remote)
            .cloned())
    }

    fn begin_media_session(&self) -> u64 {
        self.media_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn active_source_session_id(&self, id: u64, remote_ip: IpAddr) -> Option<u64> {
        self.active.lock().ok().and_then(|active| {
            active.as_ref().and_then(|session| {
                (session.id == id || session.remote_ip == remote_ip).then_some(session.id)
            })
        })
    }

    fn playback_state_for(&self, id: u64, remote_ip: IpAddr) -> u32 {
        let active = self.active.lock().ok().and_then(|active| active.clone());
        let Some(active) =
            active.filter(|session| session.id == id || session.remote_ip == remote_ip)
        else {
            return 0;
        };
        if active.playback_gate.is_paused() {
            3
        } else {
            2
        }
    }

    fn is_paused(&self) -> bool {
        self.active
            .lock()
            .ok()
            .and_then(|active| {
                active
                    .as_ref()
                    .map(|session| session.playback_gate.is_source_paused())
            })
            .unwrap_or(false)
    }

    pub(crate) fn suspend_output(&self) {
        self.output_suspended.store(true, Ordering::Release);
        if let Some(gate) = self
            .active
            .lock()
            .ok()
            .and_then(|active| active.as_ref().map(|session| session.playback_gate.clone()))
        {
            gate.set_output_suspended(true);
        }
    }

    pub(crate) fn resume_output(&self) {
        self.output_suspended.store(false, Ordering::Release);
        if let Some(gate) = self
            .active
            .lock()
            .ok()
            .and_then(|active| active.as_ref().map(|session| session.playback_gate.clone()))
        {
            gate.set_output_suspended(false);
        }
    }
}

fn allocate_notification_sequence(counter: &AtomicU32) -> u32 {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            Some(if current >= u32::from(u16::MAX) {
                1
            } else {
                current + 1
            })
        })
        .expect("notification sequence update cannot fail")
}

fn drain_matching_effects(
    effects: &mut RemoteEffectState,
    session_id: u64,
    predicate: impl Fn(&ExpectedRemoteEffect) -> bool,
) -> Vec<Sender<bool>> {
    let matching: Vec<u64> = effects
        .pending
        .iter()
        .filter_map(|(effect_id, pending)| {
            (pending.session_id == session_id && predicate(&pending.expected)).then_some(*effect_id)
        })
        .collect();
    matching
        .into_iter()
        .filter_map(|effect_id| effects.pending.remove(&effect_id))
        .map(|pending| pending.completion)
        .collect()
}

fn complete_remote_effects(completions: Vec<Sender<bool>>, confirmed: bool) {
    for completion in completions {
        let _ = completion.send(confirmed);
    }
}

fn receiver_control_boolean(key: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(key.len() + 3);
    body.push(u8::try_from(key.len()).expect("MiPlay receiver-control key is too long"));
    body.extend_from_slice(key);
    body.extend_from_slice(&[0x00, 0x01]);
    body
}

fn receiver_control_u64(key: &[u8], value: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(key.len() + 10);
    body.push(u8::try_from(key.len()).expect("MiPlay receiver-control key is too long"));
    body.extend_from_slice(key);
    body.push(0x09);
    body.extend_from_slice(&value.to_be_bytes());
    body
}

fn empty_media_info_notification() -> Vec<u8> {
    vec![
        0x0b, b'm', b'e', b'd', b'i', b'a', b'I', b'n', b'f', b'o', b'E', b'x', 0x16, 0x00, 0x00,
        0x00, 0x0c, 0x06, b'm', b'T', b'i', b't', b'l', b'e', 0x14, 0x00, 0x00, 0x00, 0x00,
    ]
}

pub fn start_control_server(
    local_ip: Ipv4Addr,
    identity: DeviceIdentity,
    output_device: Option<String>,
    volume_percent: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) -> Result<ControlHub> {
    let listener = TcpListener::bind(SocketAddr::from((local_ip, CONTROL_PORT)))
        .with_context(|| format!("bind Xiaomi MiPlay control socket {local_ip}:{CONTROL_PORT}"))?;
    listener.set_nonblocking(true)?;
    let hub = ControlHub::new(volume_percent);
    let server_hub = hub.clone();
    thread::Builder::new()
        .name("miplay-control-listener".to_owned())
        .spawn(move || {
            let ids = AtomicU64::new(1);
            while !shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, remote)) => {
                        let session_id = ids.fetch_add(1, Ordering::Relaxed);
                        if let Err(error) = server_hub.register_session(session_id, &stream) {
                            events(json!({
                                "event": "error",
                                "code": "miplay_control_session_failed",
                                "message": format!("小米妙播控制会话登记失败：{error:#}"),
                            }));
                            continue;
                        }
                        if shutdown.load(Ordering::Acquire) {
                            server_hub.shutdown_sessions();
                            continue;
                        }
                        let session_hub = server_hub.clone();
                        let session_events = Arc::clone(&events);
                        let session_identity = identity.clone();
                        let session_output_device = output_device.clone();
                        if let Err(error) = thread::Builder::new()
                            .name(format!("miplay-control-{session_id}"))
                            .spawn(move || {
                                session_events(json!({
                                    "event": "interconnect_stage",
                                    "protocol": "xiaomi_miplay",
                                    "stage": InterconnectStage::TransportConnected.as_str(),
                                    "remote": remote.to_string(),
                                    "playable": false,
                                    "vendor_attestation_verified": false,
                                    "message": format!("小米妙播发现探测已连接：{remote}"),
                                }));
                                if let Err(error) = run_session(
                                    session_id,
                                    stream,
                                    session_identity,
                                    session_output_device,
                                    session_hub.clone(),
                                    Arc::clone(&session_events),
                                ) {
                                    if is_normal_peer_disconnect(&error) {
                                        session_events(json!({
                                            "event": "interconnect_diagnostic",
                                            "protocol": "xiaomi_miplay",
                                            "stage": "probe_finished",
                                            "remote": remote.to_string(),
                                            "message": "妙播来源端结束了发现或控制连接",
                                        }));
                                    } else {
                                        session_events(json!({
                                            "event": "error",
                                            "code": "miplay_control_session_failed",
                                            "message": format!("小米妙播控制会话失败：{error:#}"),
                                        }));
                                    }
                                }
                                session_hub.deactivate(session_id);
                                session_events(json!({
                                    "event": "interconnect_stage",
                                    "protocol": "xiaomi_miplay",
                                    "stage": InterconnectStage::TransportDisconnected.as_str(),
                                    "remote": remote.to_string(),
                                    "playable": false,
                                    "vendor_attestation_verified": false,
                                }));
                            })
                        {
                            server_hub.deactivate(session_id);
                            events(json!({
                                "event": "error",
                                "code": "miplay_control_session_failed",
                                "message": format!("启动小米妙播控制会话失败：{error}"),
                            }));
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(error) => {
                        events(json!({
                            "event": "error",
                            "code": "miplay_control_accept_failed",
                            "message": format!("小米妙播控制端接受连接失败：{error}"),
                        }));
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        })
        .context("spawn Xiaomi MiPlay control listener")?;
    Ok(hub)
}

#[derive(Clone, Copy)]
enum WireMode {
    Raw,
    PlainWrapper,
    EncryptedRaw,
    EncryptedWrapper,
}

struct Outgoing {
    outer_type_override: Option<u8>,
    command: u8,
    sequence: u32,
    body: Vec<u8>,
    mode: WireMode,
    wrapper_key: &'static str,
}

impl Outgoing {
    fn raw(command: u8, sequence: u32, body: impl Into<Vec<u8>>) -> Self {
        Self {
            outer_type_override: None,
            command,
            sequence,
            body: body.into(),
            mode: WireMode::Raw,
            wrapper_key: "",
        }
    }

    fn plain_ack(command: u8, sequence: u32, body: impl Into<Vec<u8>>) -> Self {
        Self {
            outer_type_override: None,
            command,
            sequence,
            body: body.into(),
            mode: WireMode::PlainWrapper,
            wrapper_key: "ack",
        }
    }

    fn encrypted(command: u8, sequence: u32, body: impl Into<Vec<u8>>) -> Self {
        Self {
            outer_type_override: None,
            command,
            sequence,
            body: body.into(),
            mode: WireMode::EncryptedRaw,
            wrapper_key: "",
        }
    }

    fn encrypted_wrapper(
        command: u8,
        sequence: u32,
        key: &'static str,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            outer_type_override: None,
            command,
            sequence,
            body: body.into(),
            mode: WireMode::EncryptedWrapper,
            wrapper_key: key,
        }
    }

    fn encrypted_namespaced(
        outer_type: u8,
        command: u8,
        sequence: u32,
        body: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            outer_type_override: Some(outer_type),
            command,
            sequence,
            body: body.into(),
            mode: WireMode::EncryptedRaw,
            wrapper_key: "",
        }
    }
}

fn run_session(
    session_id: u64,
    stream: TcpStream,
    identity: DeviceIdentity,
    output_device: Option<String>,
    hub: ControlHub,
    events: EventEmitter,
) -> Result<()> {
    // Accepted sockets inherit the listener's nonblocking state on Windows.
    // The framed protocol uses read_exact, so restore blocking mode before
    // reading frames; otherwise a temporary empty receive buffer becomes
    // WSAEWOULDBLOCK (10035) and incorrectly tears down a healthy session.
    stream
        .set_nonblocking(false)
        .context("restore blocking MiPlay control socket")?;
    stream.set_nodelay(true).ok();
    // A paused source can legitimately leave this socket silent for longer
    // than 30 seconds. The registered shutdown clone interrupts this blocking
    // read when the listener is explicitly stopped, so an idle timeout is both
    // unnecessary and harmful here.
    stream.set_read_timeout(None).ok();
    let local = stream.local_addr().context("read MiPlay local address")?;
    let remote = stream.peer_addr().context("read MiPlay peer address")?;
    let auth_key = generate_auth_key(local, remote);
    let mut key = [0_u8; 16];
    key.copy_from_slice(&auth_key.as_bytes()[..16]);

    let (tx, rx) = mpsc::channel::<Outgoing>();
    let mut writer = stream.try_clone().context("clone MiPlay control socket")?;
    let writer_events = Arc::clone(&events);
    let writer_key = key;
    thread::Builder::new()
        .name(format!("miplay-control-writer-{session_id}"))
        .spawn(move || {
            if let Err(error) = writer_loop(&mut writer, rx, writer_key, &writer_events) {
                writer_events(json!({
                    "event": "error",
                    "code": "miplay_control_write_failed",
                    "message": format!("小米妙播控制命令发送失败：{error:#}"),
                }));
            }
        })?;

    let stable_numeric_id = numeric_device_id(&identity.device_id);
    tx.send(Outgoing::raw(DEVICE_ID, 4, stable_numeric_id.into_bytes()))?;

    let mut reader = stream;
    let mut cipher = ControlCipher::new(key);
    let mut mirror_keys: Option<StreamKeys> = None;
    let own_auth_message = random_hex_32();
    let own_auth_payload =
        format!("{{\n\t\"authMsg\": \"{own_auth_message}\" \n}} \n").into_bytes();
    let mut auth_challenge_sent = false;
    let mut pending_peer_auth_ack: Option<(u32, Vec<u8>)> = None;
    // MiPCAudio owns one unsolicited NOTIFY sequence per control session.
    // Values 5..7 are the pre-authentication mode/mediaInfoEx/state
    // announcements. The first encrypted notification is sequence 8 and is
    // normally emitted as the response to GetMediaInfo (0x14), before OPEN.
    // Receiver controls continue in the same sequence space after OPEN.
    let next_notification_sequence =
        Arc::new(AtomicU32::new(FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE));
    hub.register_control_route(
        session_id,
        remote.ip(),
        tx.clone(),
        Arc::clone(&next_notification_sequence),
    );
    let mut initial_media_info_notification_sent = false;
    loop {
        let frame = read_frame(&mut reader, &mut cipher)?;
        let (payload_key, payload_type, payload_text) = diagnostic_payload(&frame.body);
        events(json!({
            "event": "protocol_trace",
            "direction": "in",
            "outer_type": frame.outer_type,
            "command_id": frame.command,
            "sequence": frame.sequence,
            "encrypted": frame.encrypted,
            "body_length": frame.body.len(),
            "wire_body_length": frame.wire_body_length,
            "wire_body_hex": diagnostic_hex(&frame.wire_body_prefix),
            "wire_body_hex_truncated": frame.wire_body_length > DIAGNOSTIC_BYTE_LIMIT,
            "payload_key": payload_key,
            "payload_type": payload_type,
            "payload_utf8": payload_text,
        }));

        // The high byte is part of the command id, not a generic wrapper.
        // HyperOS uses this 0x04xx family on a companion socket while the
        // ordinary OPEN/media stream remains on the source session. Handling
        // it before the low-byte match prevents 0x0404 from being mistaken
        // for the normal receiver PAUSE command.
        if frame.outer_type == ALONE_MEDIA_PLAYER_NAMESPACE {
            match frame.command {
                ALONE_MEDIA_PLAYER_SET_STATE => {
                    tx.send(Outgoing::encrypted_namespaced(
                        ALONE_MEDIA_PLAYER_NAMESPACE,
                        ALONE_MEDIA_PLAYER_SET_STATE_ACK,
                        frame.sequence,
                        Vec::new(),
                    ))?;
                    if let Some(active_session_id) =
                        hub.active_source_session_id(session_id, remote.ip())
                        && let Some(playing) = decode_playback_state(&frame.body)
                        && hub.observe_playback_snapshot(active_session_id, !playing)
                    {
                        emit_playback(playing, true, &events);
                    }
                }
                ALONE_MEDIA_PLAYER_GET_STATE => {
                    let mut body = Vec::with_capacity(5);
                    body.push(0);
                    body.extend_from_slice(
                        &hub.playback_state_for(session_id, remote.ip())
                            .to_be_bytes(),
                    );
                    tx.send(Outgoing::encrypted_namespaced(
                        ALONE_MEDIA_PLAYER_NAMESPACE,
                        ALONE_MEDIA_PLAYER_GET_STATE_ACK,
                        frame.sequence,
                        body,
                    ))?;
                }
                ALONE_MEDIA_PLAYER_SET_MEDIA_INFO => {
                    tx.send(Outgoing::encrypted_namespaced(
                        ALONE_MEDIA_PLAYER_NAMESPACE,
                        ALONE_MEDIA_PLAYER_SET_MEDIA_INFO_ACK,
                        frame.sequence,
                        Vec::new(),
                    ))?;
                    if let Some(active_session_id) =
                        hub.active_source_session_id(session_id, remote.ip())
                    {
                        hub.observe_media_info(active_session_id, &frame.body);
                    }
                    // Track metadata is often sent on a companion socket
                    // before the replacement media OPEN reaches us. Always
                    // publish it so the bridge can cache it for the next
                    // session instead of leaving the previous song visible.
                    emit_media_info(&frame.body, &events);
                }
                _ => {
                    events(json!({
                        "event": "protocol_trace",
                        "direction": "unhandled",
                        "outer_type": frame.outer_type,
                        "command_id": frame.command,
                        "full_command_id":
                            (u16::from(frame.outer_type) << 8) | u16::from(frame.command),
                        "sequence": frame.sequence,
                        "body_hex": hex::encode(&frame.body[..frame.body.len().min(96)]),
                    }));
                }
            }
            continue;
        }

        match frame.command {
            GET_VERSION => {
                tx.send(Outgoing::raw(
                    GET_VERSION_ACK,
                    frame.sequence,
                    format!("{PROTOCOL_VERSION}\0").into_bytes(),
                ))?;
            }
            AUTH_ACK => {
                // MiPCAudio publishes capabilities only after the peer has
                // acknowledged DEVICE_ID.  Sending them optimistically before
                // Auth_Ack (and sending a duplicate version response) causes
                // Xiaomi clients to discard the candidate after SafetyAuth.
                tx.send(Outgoing::raw(
                    NOTIFY,
                    5,
                    vec![4, b'm', b'o', b'd', b'e', 3, 2],
                ))?;
                tx.send(Outgoing::raw(NOTIFY, 6, empty_media_info_notification()))?;
                tx.send(Outgoing::raw(
                    NOTIFY,
                    7,
                    vec![5, b's', b't', b'a', b't', b'e', 3, 0],
                ))?;
            }
            SAFETY_INFO if !frame.encrypted => {
                let response = concat!(
                    "{\n\t\"aesIvType\": \"2\",\n\t\"aesKeyType\": \"1\",\n",
                    "\t\"authAlgorithmType\": \"4\",\n\t\"authKeyType\": \"1\",\n",
                    "\t\"integrityType\": \"1\",\n\t\"result\": \"0\" \n} \n"
                );
                tx.send(Outgoing::plain_ack(
                    SAFETY_INFO_ACK,
                    frame.sequence,
                    response.as_bytes().to_vec(),
                ))?;
                // Captured MiPCAudio ordering is a mutual challenge exchange:
                //   C SafetyInfo
                //   S SafetyInfoAck + SafetyAuth(challenge)
                //   C SafetyAuth(challenge) + SafetyAuthAck
                //   S SafetyAuthAck
                // Sending our challenge only after the peer's SafetyAuth, or
                // sending our final ack before verifying the peer's ack, makes
                // current Xiaomi clients close the socket immediately.
                tx.send(Outgoing::encrypted_wrapper(
                    SAFETY_AUTH,
                    0,
                    "cmd",
                    own_auth_payload.clone(),
                ))?;
                auth_challenge_sent = true;
                events(json!({
                    "event": "authentication_diagnostic",
                    "stage": "safety_challenge_sent",
                    "local_endpoint": local.to_string(),
                    "remote_endpoint": remote.to_string(),
                    "auth_key": auth_key,
                    "own_auth_message": own_auth_message,
                }));
            }
            SAFETY_AUTH => {
                let value = parse_json_payload(&frame.body).unwrap_or(Value::Null);
                let peer_auth = value
                    .get("authMsg")
                    .and_then(Value::as_str)
                    .context("SafetyAuth is missing authMsg")?;
                let ack = hmac_sha256_hex(auth_key.as_bytes(), peer_auth.as_bytes());
                events(json!({
                    "event": "authentication_diagnostic",
                    "stage": "safety_auth_received",
                    "local_endpoint": local.to_string(),
                    "remote_endpoint": remote.to_string(),
                    "auth_key": auth_key,
                    "peer_auth_message": peer_auth,
                    "own_auth_message": own_auth_message,
                    "peer_auth_ack": ack,
                }));
                // Defensive compatibility for senders that omit SafetyInfo.
                // The normal MiPCAudio-compatible path already sent this
                // challenge immediately after SafetyInfoAck.
                if !auth_challenge_sent {
                    tx.send(Outgoing::encrypted_wrapper(
                        SAFETY_AUTH,
                        0,
                        "cmd",
                        own_auth_payload.clone(),
                    ))?;
                    auth_challenge_sent = true;
                }
                let response =
                    format!("{{\n\t\"authMsgAck\": \"{ack}\",\n\t\"result\": \"0\" \n}} \n");
                // Do not send the final ack yet. MiPCAudio first verifies the
                // peer's SafetyAuthAck for our challenge, then acknowledges
                // the peer challenge. This ordering is observable in cap4.
                pending_peer_auth_ack = Some((frame.sequence, response.into_bytes()));
            }
            SAFETY_AUTH_ACK => {
                if let Some(received) = parse_json_payload(&frame.body).and_then(|value| {
                    value
                        .get("authMsgAck")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }) {
                    let expected =
                        hmac_sha256_hex(auth_key.as_bytes(), own_auth_message.as_bytes());
                    events(json!({
                        "event": "authentication_diagnostic",
                        "stage": "safety_auth_ack_received",
                        "received_auth_ack": received,
                        "expected_auth_ack": expected,
                        "matches": received == expected,
                    }));
                    if received != expected {
                        bail!("SafetyAuth_Ack HMAC mismatch");
                    }
                    let (peer_sequence, response) = pending_peer_auth_ack
                        .take()
                        .context("SafetyAuth_Ack arrived before SafetyAuth")?;
                    tx.send(Outgoing::encrypted_wrapper(
                        SAFETY_AUTH_ACK,
                        peer_sequence,
                        "ack",
                        response,
                    ))?;
                    events(json!({
                        "event": "status",
                        "state": "secure_channel_established",
                        "message": "小米妙播链路鉴权已通过，正在等待设备准入和媒体会话",
                    }));
                    events(json!({
                        "event": "interconnect_stage",
                        "protocol": "xiaomi_miplay",
                        "stage": InterconnectStage::SecureChannelEstablished.as_str(),
                        "remote": remote.to_string(),
                        "playable": false,
                        "authentication_scope": "legacy_session_hmac",
                        "vendor_attestation_verified": false,
                    }));
                }
            }
            GET_DEVICE_INFO => {
                tx.send(Outgoing::encrypted(
                    GET_DEVICE_INFO_ACK,
                    frame.sequence,
                    build_device_info(&identity),
                ))?;
                events(json!({
                    "event": "interconnect_stage",
                    "protocol": "xiaomi_miplay",
                    "stage": InterconnectStage::IdentityExchanged.as_str(),
                    "remote": remote.to_string(),
                    "playable": false,
                    "identity_provenance": "local_system",
                    "account_bound": false,
                    "vendor_attestation_verified": false,
                }));
            }
            SET_DEVICE_INFO => {
                tx.send(Outgoing::encrypted(
                    SET_DEVICE_INFO_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
                emit_source_info(&frame.body, &events);
                events(json!({
                    "event": "interconnect_stage",
                    "protocol": "xiaomi_miplay",
                    "stage": InterconnectStage::CapabilitiesExchanged.as_str(),
                    "remote": remote.to_string(),
                    "playable": false,
                    "vendor_attestation_verified": false,
                }));
            }
            GET_MIRROR_MODE => {
                tx.send(Outgoing::encrypted(
                    GET_MIRROR_MODE_ACK,
                    frame.sequence,
                    // Mode 1 is Xiaomi's active mobile-audio-streaming route.
                    // Mode 2 can carry audio but HyperOS rejects receiver-side
                    // media controls because that route has no mobile stream.
                    MOBILE_AUDIO_STREAMING_MODE_ACK.to_vec(),
                ))?;
            }
            SET_MIRROR_KEY => {
                let value = parse_json_payload(&frame.body).context("parse SetMirrorKey JSON")?;
                let get = |name: &str| {
                    value
                        .get(name)
                        .and_then(Value::as_str)
                        .with_context(|| format!("SetMirrorKey is missing {name}"))
                };
                let keys =
                    StreamKeys::from_strings(get("authKey")?, get("streamKey")?, get("streamIV")?)?;
                mirror_keys = Some(keys.clone());
                hub.remember_stream_keys(remote.ip(), session_id, keys.clone())?;
                events(json!({
                    "event": "stream_keys_ready",
                    "protocol": "xiaomi_miplay",
                    "remote": remote.to_string(),
                    "session_id": session_id,
                    "key_fingerprint": keys.fingerprint(),
                }));
                tx.send(Outgoing::encrypted(
                    SET_MIRROR_KEY_ACK,
                    frame.sequence,
                    vec![0],
                ))?;
            }
            GET_VOLUME => {
                let percent = hub.volume_percent.load(Ordering::Acquire).min(100);
                tx.send(Outgoing::encrypted(
                    GET_VOLUME_ACK,
                    frame.sequence,
                    encode_get_volume_ack(percent),
                ))?;
                emit_volume(percent, &events);
            }
            HEART_BEAT => {
                // CmdSource::sendHeartBeat uses command 0x1a and requires the
                // matching 0x1b reply with the same sequence. HyperOS sends a
                // heartbeat immediately after consuming reverse-control
                // notifications, so leaving it unhandled makes an otherwise
                // valid phone-side pause/next request appear to hang and then
                // tears down the control route.
                tx.send(Outgoing::encrypted(
                    HEART_BEAT_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
            }
            GET_STATE => {
                // MiPCAudio answers the pre-OPEN probe with idle (0), then
                // reports playing (2) or paused (3) after OPEN. Returning
                // idle forever makes HyperOS discard state transitions after
                // a reverse-control notification.
                let mut body = Vec::with_capacity(5);
                body.push(0);
                body.extend_from_slice(
                    &hub.playback_state_for(session_id, remote.ip())
                        .to_be_bytes(),
                );
                tx.send(Outgoing::encrypted(GET_STATE_ACK, frame.sequence, body))?;
            }
            SET_PLAY_SOURCE => {
                // HyperOS publishes the Control Center source context before
                // it accepts receiver-originated key-pause/key-next NOTIFY
                // values. MiPCAudio acknowledges this registration with 0x41.
                tx.send(Outgoing::encrypted(
                    SET_PLAY_SOURCE_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
                hub.mark_play_source_registered(session_id);
                events(json!({
                    "event": "play_source",
                    "source": "xiaomi",
                    "context": parse_json_payload(&frame.body),
                }));
            }
            OPEN if frame.encrypted => {
                let url = String::from_utf8_lossy(&frame.body)
                    .trim_matches(char::from(0))
                    .to_owned();
                let (host, port) = parse_wfd_url(&url)?;
                let (keys, key_session_id, key_scope) = if let Some(keys) = mirror_keys.clone() {
                    (Some(keys), Some(session_id), "same_control_session")
                } else if let Some(shared) = hub.stream_keys_for(remote.ip())? {
                    (
                        Some(shared.keys),
                        Some(shared.session_id),
                        "shared_source_address",
                    )
                } else {
                    // TV receivers do not participate in the Xiaomi-account
                    // PC key exchange. HyperOS therefore opens the WFD/RTSP
                    // route directly and sends an unprotected MPEG-TS audio
                    // stream. Requiring SetMirrorKey here incorrectly turns a
                    // valid TV session into an immediate disconnect.
                    (None, None, "tv_unprotected_stream")
                };
                events(json!({
                    "event": "media_key_selected",
                    "protocol": "xiaomi_miplay",
                    "remote": remote.to_string(),
                    "open_session_id": session_id,
                    "key_session_id": key_session_id,
                    "key_scope": key_scope,
                    "key_fingerprint": keys.as_ref().map(StreamKeys::fingerprint),
                    "encrypted_media_expected": keys.is_some(),
                }));
                tx.send(Outgoing::encrypted(
                    OPEN_ACK,
                    frame.sequence,
                    vec![0, 0, 0, 0, 0],
                ))?;
                // Current HyperOS normally requests media info before OPEN.
                // Keep a defensive fallback for older senders that omit the
                // request, while never publishing the same sequence twice.
                if !initial_media_info_notification_sent {
                    let sequence = allocate_notification_sequence(&next_notification_sequence);
                    tx.send(Outgoing::encrypted(
                        NOTIFY,
                        sequence,
                        empty_media_info_notification(),
                    ))?;
                    initial_media_info_notification_sent = true;
                }
                // A background discovery probe is not an active media route.
                // Publish the session to playback controls only after the
                // source explicitly opens a media stream.
                let media_generation = hub.begin_media_session();
                let media_paused = hub.activate(
                    session_id,
                    remote.ip(),
                    tx.clone(),
                    Arc::clone(&next_notification_sequence),
                );
                spawn_rtsp_receiver(
                    host,
                    port,
                    keys,
                    output_device.clone(),
                    media_paused,
                    Arc::clone(&hub.volume_percent),
                    media_generation,
                    Arc::clone(&hub.media_generation),
                    Arc::clone(&events),
                );
                events(json!({
                    "event": "status",
                    "state": "media_opening",
                    "message": format!("正在打开小米妙播媒体流：{host}:{port}"),
                }));
                events(json!({
                    "event": "interconnect_stage",
                    "protocol": "xiaomi_miplay",
                    "stage": InterconnectStage::MediaSessionEstablished.as_str(),
                    "remote": remote.to_string(),
                    "media_generation": media_generation,
                    "playable": true,
                    "route_acceptance": "source_open_command",
                    "vendor_attestation_verified": false,
                }));
            }
            PAUSE => {
                tx.send(Outgoing::encrypted(PAUSE_ACK, frame.sequence, Vec::new()))?;
                if let Some(active_session_id) =
                    hub.active_source_session_id(session_id, remote.ip())
                {
                    hub.observe_playback(active_session_id, true);
                    emit_playback(false, true, &events);
                }
            }
            RESUME => {
                tx.send(Outgoing::encrypted(RESUME_ACK, frame.sequence, Vec::new()))?;
                if let Some(active_session_id) =
                    hub.active_source_session_id(session_id, remote.ip())
                {
                    hub.observe_playback(active_session_id, false);
                    emit_playback(true, true, &events);
                }
            }
            PAUSE_ACK
            | RESUME_ACK
            | PAUSE_MEDIA_PLAYER_ACK
            | RESUME_MEDIA_PLAYER_ACK
            | SEEK_MEDIA_PLAYER_ACK
            | PREVIOUS_MEDIA_PLAYER_ACK
            | NEXT_MEDIA_PLAYER_ACK => {
                events(json!({
                    "event": "media_control_acknowledgement",
                    "protocol": "xiaomi_miplay",
                    "command_id": frame.command,
                    "sequence": frame.sequence,
                    "accepted": false,
                    "reason": "receiver_notify_controls_are_confirmed_by_remote_state",
                    "body_hex": diagnostic_hex(&frame.body),
                }));
            }
            SET_VOLUME => {
                let percent = decode_volume_percent(&frame.body);
                // MiPCAudio acknowledges SET_VOLUME with an empty body. HyperOS
                // treats this command as a status-only acknowledgement, so the
                // applied value must not be placed in the ACK body.
                hub.volume_percent.store(percent, Ordering::Release);
                tx.send(Outgoing::encrypted(
                    SET_VOLUME_ACK,
                    frame.sequence,
                    encode_set_volume_ack(),
                ))?;
                // HyperOS keeps a separate receiver-volume cache and restores it
                // shortly after a slider gesture unless the receiver publishes
                // the applied value. Keep the ACK first, then mirror MiPCAudio's
                // type-7 big-endian `volume` notification wire format.
                let sequence = allocate_notification_sequence(&next_notification_sequence);
                tx.send(Outgoing::encrypted(
                    NOTIFY,
                    sequence,
                    volume_notification(percent),
                ))?;
                // HyperOS may set the remote volume while the media route is
                // still opening. Preserve that value so the first PCM samples
                // and every later GET_VOLUME observe one coherent state.
                emit_volume(percent, &events);
            }
            SET_MEDIA_INFO => {
                tx.send(Outgoing::encrypted(
                    SET_MEDIA_INFO_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
                if let Some(active_session_id) =
                    hub.active_source_session_id(session_id, remote.ip())
                {
                    // HyperOS commonly opens one media route and publishes
                    // metadata on a companion control socket from the same
                    // phone. Treat both sockets as one source so a track
                    // change cannot be filtered merely because its SET_MEDIA_INFO
                    // arrived on the non-OPEN session.
                    hub.observe_media_info(active_session_id, &frame.body);
                }
                // Xiaomi may publish the next track before its replacement
                // OPEN/session state. Forward the metadata independently of
                // the current media socket; XiaomiPlaybackReducer will bind
                // it to the active or immediately pending session in order.
                emit_media_info(&frame.body, &events);
            }
            GET_MEDIA_INFO => {
                // MiPCAudio does not answer GetMediaInfo with command 0x15.
                // It publishes the receiver's `mediaInfoEx` value through an
                // unsolicited NOTIFY instead.  HyperOS installs the reverse
                // media-control callback while consuming this notification;
                // delaying it until OPEN makes key-pause/key-next/etc. vanish
                // without an acknowledgement on current phones.
                let sequence = allocate_notification_sequence(&next_notification_sequence);
                tx.send(Outgoing::encrypted(
                    NOTIFY,
                    sequence,
                    empty_media_info_notification(),
                ))?;
                hub.mark_reverse_control_ready(session_id);
                initial_media_info_notification_sent = true;
            }
            SET_POSITION => {
                if let Some(active_session_id) =
                    hub.active_source_session_id(session_id, remote.ip())
                    && frame.body.len() >= 8
                {
                    let position_ms = u64::from_be_bytes(frame.body[..8].try_into().unwrap());
                    let (changed, resumed) = hub.observe_position(active_session_id, position_ms);
                    if resumed {
                        emit_playback(true, true, &events);
                    }
                    if changed {
                        events(json!({
                            "event": "progress",
                            "source": "xiaomi",
                            "position_ms": position_ms,
                        }));
                    }
                }
                tx.send(Outgoing::encrypted(
                    SET_POSITION_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
            }
            SET_MEDIA_STATE => {
                tx.send(Outgoing::encrypted(
                    SET_MEDIA_STATE_ACK,
                    frame.sequence,
                    Vec::new(),
                ))?;
                if let Some(active_session_id) =
                    hub.active_source_session_id(session_id, remote.ip())
                    && let Some(playing) = decode_playback_state(&frame.body)
                    && hub.observe_playback_snapshot(active_session_id, !playing)
                {
                    emit_playback(playing, true, &events);
                }
            }
            _ => {
                events(json!({
                    "event": "protocol_trace",
                    "direction": "unhandled",
                    "outer_type": frame.outer_type,
                    "command_id": frame.command,
                    "full_command_id":
                        (u16::from(frame.outer_type) << 8) | u16::from(frame.command),
                    "sequence": frame.sequence,
                    "body_hex": hex::encode(&frame.body[..frame.body.len().min(96)]),
                }));
            }
        }
    }
}

fn writer_loop(
    stream: &mut TcpStream,
    receiver: Receiver<Outgoing>,
    key: [u8; 16],
    events: &EventEmitter,
) -> Result<()> {
    let mut cipher = ControlCipher::new(key);
    while let Ok(outgoing) = receiver.recv() {
        let plaintext_body_hex = diagnostic_hex(&outgoing.body);
        let plaintext_body_utf8 = diagnostic_utf8(&outgoing.body);
        let wrapper_key = outgoing.wrapper_key;
        let (default_outer_type, body) = match outgoing.mode {
            WireMode::Raw => (0x00, outgoing.body),
            WireMode::PlainWrapper => (0x14, wrap_payload(outgoing.wrapper_key, &outgoing.body)?),
            WireMode::EncryptedRaw => (0x00, cipher.encrypt(&outgoing.body)?),
            WireMode::EncryptedWrapper => {
                let plain = wrap_payload(outgoing.wrapper_key, &outgoing.body)?;
                // MiPCAudio keeps the outer wrapper marker for SafetyAuth and
                // SafetyAuth_Ack even though their payload is AES encrypted.
                // Sending 0x00 here makes Xiaomi clients reject the receiver
                // during their background compatibility probe.
                (0x14, cipher.encrypt(&plain)?)
            }
        };
        let outer_type = outgoing.outer_type_override.unwrap_or(default_outer_type);
        events(json!({
            "event": "protocol_trace",
            "direction": "out",
            "outer_type": outer_type,
            "command_id": outgoing.command,
            "sequence": outgoing.sequence,
            "body_length": body.len(),
            "encrypted": body.starts_with(&[0x00, 0x07, 0x01, 0xe0]),
            "wrapper_key": if wrapper_key.is_empty() { None } else { Some(wrapper_key) },
            "plaintext_body_hex": plaintext_body_hex,
            "plaintext_body_utf8": plaintext_body_utf8,
            "wire_body_hex": diagnostic_hex(&body),
            "wire_body_hex_truncated": body.len() > DIAGNOSTIC_BYTE_LIMIT,
        }));
        write_frame(
            stream,
            outer_type,
            outgoing.command,
            outgoing.sequence,
            &body,
        )?;
    }
    Ok(())
}

fn is_normal_peer_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::TimedOut
                )
            })
    })
}

struct Incoming {
    outer_type: u8,
    command: u8,
    sequence: u32,
    encrypted: bool,
    body: Vec<u8>,
    wire_body_length: usize,
    wire_body_prefix: Vec<u8>,
}

fn read_frame(stream: &mut TcpStream, cipher: &mut ControlCipher) -> Result<Incoming> {
    let mut header = [0_u8; 9];
    stream
        .read_exact(&mut header)
        .context("read MiPlay control header")?;
    if header[0] != 0x24 {
        bail!("invalid MiPlay control header {}", hex::encode(header));
    }
    // Official CmdSource::getCmdData writes the header as:
    //   '$' | command:u16(be) | value_type:u16(be) | length:u32(be)
    // The command high byte is the wrapper namespace (0x00/0x14). Older
    // FusionPlay builds mistook value_type's high byte for a reserved zero and
    // the final two length bytes for the whole length. HyperOS eventually uses
    // value_type 0x0100, at which point that parser terminated a valid session.
    let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if length > MAX_CONTROL_BODY_LENGTH {
        bail!("invalid MiPlay body length {length}");
    }
    let mut body = vec![0_u8; length];
    stream
        .read_exact(&mut body)
        .context("read MiPlay control body")?;
    let wire_body_length = body.len();
    let wire_body_prefix = body[..body.len().min(DIAGNOSTIC_BYTE_LIMIT)].to_vec();
    let encrypted = body.starts_with(&[0x00, 0x07, 0x01, 0xe0]);
    if encrypted {
        body = cipher.decrypt(&body)?;
    } else if header[1] == 0x14 {
        body = unwrap_payload(&body)?.2;
    }
    Ok(Incoming {
        outer_type: header[1],
        command: header[2],
        sequence: u32::from(u16::from_be_bytes([header[3], header[4]])),
        encrypted,
        body,
        wire_body_length,
        wire_body_prefix,
    })
}

const DIAGNOSTIC_BYTE_LIMIT: usize = 512;
const MAX_CONTROL_BODY_LENGTH: usize = 16 * 1024 * 1024;

fn diagnostic_hex(bytes: &[u8]) -> String {
    hex::encode(&bytes[..bytes.len().min(DIAGNOSTIC_BYTE_LIMIT)])
}

fn diagnostic_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(DIAGNOSTIC_BYTE_LIMIT)]).into_owned()
}

fn diagnostic_payload(body: &[u8]) -> (Option<String>, Option<u32>, String) {
    if let Ok((key, payload_type, payload)) = unwrap_payload(body) {
        return (Some(key), Some(payload_type), diagnostic_utf8(&payload));
    }
    (None, None, diagnostic_utf8(body))
}

fn write_frame(
    stream: &mut TcpStream,
    outer_type: u8,
    command: u8,
    sequence: u32,
    body: &[u8],
) -> Result<()> {
    if body.len() > MAX_CONTROL_BODY_LENGTH {
        bail!("MiPlay control body is too large: {} bytes", body.len());
    }
    let length = u32::try_from(body.len()).context("MiPlay control body is too large")?;
    let value_type = u16::try_from(sequence).context("MiPlay value type exceeds 16 bits")?;
    let header = [
        0x24,
        outer_type,
        command,
        (value_type >> 8) as u8,
        value_type as u8,
        (length >> 24) as u8,
        (length >> 16) as u8,
        (length >> 8) as u8,
        length as u8,
    ];
    stream.write_all(&header)?;
    stream.write_all(body)?;
    stream.flush().context("flush MiPlay control frame")
}

struct ControlCipher {
    key: [u8; 16],
    iv: [u8; 16],
}

impl ControlCipher {
    fn new(key: [u8; 16]) -> Self {
        Self { key, iv: key }
    }

    fn encrypt(&mut self, plain: &[u8]) -> Result<Vec<u8>> {
        let pad = 16 - (plain.len() % 16);
        let mut padded = Vec::with_capacity(plain.len() + pad);
        padded.extend_from_slice(plain);
        // Xiaomi records the padding length in the safety envelope but fills
        // the AES block tail with zeroes rather than PKCS#7 bytes.  This is
        // covered by a byte-for-byte MiPCAudio capture regression test below.
        padded.resize(plain.len() + pad, 0);
        let encryptor = Encryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
            .context("initialize MiPlay control encryption")?;
        let padded_len = padded.len();
        encryptor
            .encrypt_padded::<NoPadding>(&mut padded, padded_len)
            .map_err(|_| anyhow::anyhow!("encrypt MiPlay control frame"))?;
        self.iv.copy_from_slice(&padded[padded.len() - 16..]);
        let mut output = Vec::with_capacity(9 + padded.len());
        output.extend_from_slice(&[0x00, 0x07, 0x01, 0xe0, pad as u8]);
        // integrityType=1 is Xiaomi's byte-swapped, table-driven CRC-32.
        // MiPlay validates this field before it attempts to decrypt the
        // SafetyAuth payload, so random placeholder bytes make the phone
        // silently close the control socket after sending its challenge.
        output.extend_from_slice(&miplay_integrity(&padded).to_be_bytes());
        output.extend_from_slice(&padded);
        Ok(output)
    }

    fn decrypt(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        if body.len() < 25 || !(body.len() - 9).is_multiple_of(16) {
            bail!("invalid encrypted MiPlay body length {}", body.len());
        }
        let padding = usize::from(body[4]);
        let mut ciphertext = body[9..].to_vec();
        let received_integrity = u32::from_be_bytes(body[5..9].try_into().unwrap());
        let expected_integrity = miplay_integrity(&ciphertext);
        if received_integrity != expected_integrity {
            bail!(
                "MiPlay encrypted-body integrity mismatch: received {received_integrity:08x}, expected {expected_integrity:08x}"
            );
        }
        let next_iv: [u8; 16] = ciphertext[ciphertext.len() - 16..].try_into().unwrap();
        let decryptor = Decryptor::<Aes128>::new_from_slices(&self.key, &self.iv)
            .context("initialize MiPlay control decryption")?;
        decryptor
            .decrypt_padded::<NoPadding>(&mut ciphertext)
            .map_err(|_| anyhow::anyhow!("decrypt MiPlay control frame"))?;
        self.iv = next_iv;
        if padding == 0 || padding > 16 || padding > ciphertext.len() {
            bail!("invalid MiPlay zero-padding length {padding}");
        }
        ciphertext.truncate(ciphertext.len() - padding);
        Ok(ciphertext)
    }
}

/// MiPlay integrityType=1, recovered from MiPCAudio's SafetyDataDeal.
///
/// Its lookup table is the ordinary non-reflected CRC-32 polynomial
/// 0x04c11db7 with every table word byte-swapped. The running value starts at
/// 0xffffffff and is intentionally returned without the usual final XOR.
fn miplay_integrity(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        let index = (value as u8) ^ byte;
        value = miplay_integrity_table_entry(index) ^ (value >> 8);
    }
    value
}

fn miplay_integrity_table_entry(index: u8) -> u32 {
    let mut value = u32::from(index) << 24;
    for _ in 0..8 {
        value = if value & 0x8000_0000 != 0 {
            (value << 1) ^ 0x04c1_1db7
        } else {
            value << 1
        };
    }
    value.swap_bytes()
}

fn wrap_payload(key: &str, data: &[u8]) -> Result<Vec<u8>> {
    if key.len() > u8::MAX as usize || data.len() > u8::MAX as usize {
        bail!("MiPlay wrapped payload exceeds one-byte length field");
    }
    let mut body = Vec::with_capacity(key.len() + data.len() + 6);
    body.push(key.len() as u8);
    body.extend_from_slice(key.as_bytes());
    body.extend_from_slice(&30_u32.to_le_bytes());
    body.push(data.len() as u8);
    body.extend_from_slice(data);
    Ok(body)
}

fn unwrap_payload(body: &[u8]) -> Result<(String, u32, Vec<u8>)> {
    let key_len = usize::from(*body.first().context("empty MiPlay wrapper")?);
    if body.len() < key_len + 6 {
        bail!("truncated MiPlay wrapper");
    }
    let key = String::from_utf8_lossy(&body[1..1 + key_len]).to_string();
    let data_type = u32::from_le_bytes(body[1 + key_len..5 + key_len].try_into().unwrap());
    let data_len = usize::from(body[5 + key_len]);
    if body.len() < 6 + key_len + data_len {
        bail!("truncated MiPlay wrapped data");
    }
    Ok((
        key,
        data_type,
        body[6 + key_len..6 + key_len + data_len].to_vec(),
    ))
}

fn generate_auth_key(local: SocketAddr, remote: SocketAddr) -> String {
    let value = format!(
        "{}{}{}{}",
        local.ip(),
        local.port(),
        remote.ip(),
        remote.port()
    );
    let mapped: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_digit() {
                char::from_u32(character as u32 + 0x31).unwrap()
            } else {
                character
            }
        })
        .collect();
    hex::encode(Md5::digest(mapped.as_bytes()))
}

fn numeric_device_id(device_id: &str) -> String {
    let digest = Md5::digest(device_id.as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    format!("{:014}", u64::from_be_bytes(bytes) % 100_000_000_000_000)
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary keys");
    hmac.update(message);
    hex::encode(hmac.finalize().into_bytes())
}

fn random_hex_32() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn parse_json_payload(body: &[u8]) -> Option<Value> {
    let candidate = if body.first().is_some_and(|byte| *byte <= 8) {
        unwrap_payload(body)
            .ok()
            .map(|(_, _, data)| data)
            .unwrap_or_else(|| body.to_vec())
    } else {
        body.to_vec()
    };
    serde_json::from_slice(&candidate).ok()
}

fn parse_wfd_url(value: &str) -> Result<(Ipv4Addr, u16)> {
    let authority = value
        .strip_prefix("wfd://")
        .context("Open URL does not start with wfd://")?
        .split('?')
        .next()
        .unwrap_or("");
    let (host, port) = authority
        .rsplit_once(':')
        .context("Open URL is missing a port")?;
    Ok((
        host.parse().context("parse Open IPv4 address")?,
        port.parse().context("parse Open RTSP port")?,
    ))
}

fn build_device_info(identity: &DeviceIdentity) -> Vec<u8> {
    // Keep a single canonical route identity. Extra aliases such as `miName`,
    // `sn` and `sourceName` make current HyperOS builds retain the Lyra and
    // IDM records as separate routes instead of merging them into one target.
    let device_id = short_device_id(&identity.device_id);
    let device_type = identity.device_type.protocol_value().to_string();
    let entries = [
        ("alonePlayCapacity", "0"),
        ("canAlonePlayCtrl", "0"),
        ("canHeadsetCtrl", "0"),
        ("canRevCtrl", "1"),
        ("channel", ""),
        ("deviceId", device_id.as_str()),
        // Keep the authenticated command session aligned with the category
        // selected by the user and published by `_mi-connect` discovery.
        ("deviceType", device_type.as_str()),
        ("model", identity.model.as_str()),
        ("needAblum", "1"),
        ("needLrc", "1"),
        ("needPos", "1"),
        ("romVersion", ""),
        ("support", "audio"),
    ];
    // MiPCAudio's command-31 payload begins with this value envelope. The
    // Android native command layer consumes it before forwarding the key/value
    // table to DeviceManager.analysisDeviceInfo. Omitting it shifts the table
    // by three bytes, leaving deviceId/deviceType unset and turning the first
    // usable callback into an invalid deviceUpdate instead of deviceFound.
    let mut body = vec![0x00, 0x01, 0x55];
    for (key, value) in entries {
        let key_bytes = key.as_bytes();
        let value_bytes = value.as_bytes();
        body.push(key_bytes.len() as u8);
        body.extend_from_slice(key_bytes);
        body.push(0x0c);
        body.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());
        body.extend_from_slice(value_bytes);
    }
    body
}

fn short_device_id(device_id: &str) -> String {
    // MiPCAudio uses the same three-character account-scoped identifier in
    // both `_mi-connect idHash` and the post-authentication GetDeviceInfo
    // response.  Keep the normal machine-derived identity for production,
    // while allowing packet-comparison diagnostics to reproduce an observed
    // account identity consistently across both discovery and control.
    if let Ok(value) = std::env::var("FUSIONPLAY_MIPLAY_DIAGNOSTIC_ACCOUNT_ID") {
        let value = value.trim();
        if value.len() == 3
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return value.to_owned();
        }
    }
    if device_id.len() == 3
        && device_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return device_id.to_owned();
    }
    let hex_digits: String = device_id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .map(|character| character.to_ascii_uppercase())
        .collect();
    if hex_digits.len() >= 8 {
        return hex_digits[hex_digits.len() - 8..].to_owned();
    }
    hex::encode_upper(&Md5::digest(device_id.as_bytes())[..4])
}

fn emit_source_info(body: &[u8], events: &EventEmitter) {
    if let Some(value) = parse_json_payload(body)
        && let Some(name) = value.get("sourceName").and_then(Value::as_str)
    {
        events(json!({
            "event": "source_info",
            "source": "xiaomi",
            "source_name": name,
            "raw": value,
        }));
    }
}

fn json_string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    })
}

fn json_u64_field(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|field| {
            field
                .as_u64()
                .or_else(|| field.as_str().and_then(|text| text.trim().parse().ok()))
        })
    })
}

fn media_info_value(body: &[u8]) -> Option<Value> {
    let value = parse_json_payload(body)?;
    for key in ["mediaInfo", "mediaInfoEx", "metadata"] {
        match value.get(key) {
            Some(Value::Object(nested)) => return Some(Value::Object(nested.clone())),
            Some(Value::String(nested)) => {
                if let Ok(parsed) = serde_json::from_str(nested) {
                    return Some(parsed);
                }
            }
            _ => {}
        }
    }
    Some(value)
}

fn media_identity(body: &[u8]) -> Option<String> {
    let value = media_info_value(body)?;
    let id =
        json_string_field(&value, &["mAudioId", "audioId", "mId", "id", "mediaId"]).unwrap_or("");
    let title = json_string_field(&value, &["mTitle", "title"]).unwrap_or("");
    let artist = json_string_field(&value, &["mArtist", "artist"]).unwrap_or("");
    let album = json_string_field(&value, &["mAlbum", "album"]).unwrap_or("");
    if id.is_empty() && title.is_empty() && artist.is_empty() && album.is_empty() {
        return None;
    }
    Some(format!("{id}\u{1f}{title}\u{1f}{artist}\u{1f}{album}"))
}

fn emit_media_info(body: &[u8], events: &EventEmitter) {
    let Some(value) = media_info_value(body) else {
        return;
    };
    let title = json_string_field(&value, &["mTitle", "title"]).unwrap_or("");
    let artist = json_string_field(&value, &["mArtist", "artist"]).unwrap_or("");
    let album = json_string_field(&value, &["mAlbum", "album"]).unwrap_or("");
    let duration_ms = json_u64_field(
        &value,
        &["mDuration", "duration", "durationMs", "duration_ms"],
    )
    .unwrap_or(0);
    // MediaMetaData.volume is not a receiver-volume notification. HyperOS and
    // several sender apps serialize it as zero when the field is unspecified.
    // Treating that placeholder as authoritative repeatedly muted the native
    // audio gain after connection and after every metadata refresh. Receiver
    // volume is synchronized exclusively through SET_VOLUME / GET_VOLUME.
    // Xiaomi's public MediaMetaData model serializes the stable song id as
    // `mAudioId` (and older TV senders may use `mId`).  Falling back to a
    // title-derived id is still useful for sources which omit both fields,
    // but that fallback must not include duration: duration commonly arrives
    // in a later partial update and previously made one song look like two.
    let explicit_track_id =
        json_string_field(&value, &["mAudioId", "audioId", "mId", "id", "mediaId"]);
    let track_id = explicit_track_id.map(str::to_owned).or_else(|| {
        if title.is_empty() && artist.is_empty() && album.is_empty() {
            return None;
        }
        let mut digest = Sha256::new();
        for identity_part in [title, artist, album] {
            digest.update(identity_part.trim().as_bytes());
            digest.update([0]);
        }
        Some(format!(
            "metadata:{}",
            hex::encode(&digest.finalize()[..16])
        ))
    });
    let artwork = json_string_field(
        &value,
        &["mArt", "mCoverUrl", "artwork", "artworkUrl", "coverUrl"],
    )
    .map(|artwork| Value::String(artwork.to_owned()))
    .unwrap_or(Value::Null);
    events(json!({
        "event": "media_info",
        "source": "xiaomi",
        "track_id": track_id,
        "title": title,
        "artist": artist,
        "album": album,
        "duration_ms": duration_ms,
        "position_ms": json_u64_field(
            &value,
            &["mPosition", "position", "positionMs", "position_ms"],
        ),
        "artwork": artwork,
        "metadata_change_type": json_u64_field(
            &value,
            &["mMetaChangeType", "metaChangeType"],
        ),
    }));
}

fn decode_playback_state(body: &[u8]) -> Option<bool> {
    let state = if body.len() == 4 {
        Some(u32::from_be_bytes(body.try_into().ok()?))
    } else if body.len() >= 5 && body[0] == 0 {
        Some(u32::from_be_bytes(body[1..5].try_into().ok()?))
    } else {
        let value = parse_json_payload(body)?;
        ["setState", "state", "mediaState", "playState"]
            .into_iter()
            .find_map(|name| {
                value.get(name).and_then(|field| {
                    field
                        .as_u64()
                        .or_else(|| field.as_str()?.trim().parse().ok())
                })
            })
            .and_then(|state| u32::try_from(state).ok())
    }?;
    match state {
        2 => Some(true),
        3 => Some(false),
        _ => None,
    }
}

fn decode_volume_percent(body: &[u8]) -> u32 {
    if body.len() < 4 {
        return 100;
    }
    i32::from_be_bytes(body[..4].try_into().unwrap()).clamp(0, 100) as u32
}

fn encode_get_volume_ack(percent: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(5);
    body.push(0);
    body.extend_from_slice(&percent.min(100).to_be_bytes());
    body
}

fn encode_set_volume_ack() -> Vec<u8> {
    Vec::new()
}

fn volume_notification(percent: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.push(6);
    body.extend_from_slice(b"volume");
    body.push(7);
    body.extend_from_slice(&percent.min(100).to_be_bytes());
    body
}

fn emit_volume(percent: u32, events: &EventEmitter) {
    events(json!({
        "event": "volume",
        "source": "xiaomi",
        "percent": percent.min(100),
    }));
}

fn emit_playback(playing: bool, session_active: bool, events: &EventEmitter) {
    events(json!({
        "event": "playback_state",
        "raw_state": if playing { 2 } else { 3 },
        "playing": playing,
        "session_active": session_active,
    }));
}

#[cfg(test)]
mod tests {
    use super::{
        ControlCipher, ControlHub, DeviceIdentity, FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE,
        GET_MEDIA_INFO, MOBILE_AUDIO_STREAMING_MODE_ACK, MediaAction, MediaControlOutcome, NOTIFY,
        SAFETY_AUTH, SAFETY_AUTH_ACK, SAFETY_INFO, SAFETY_INFO_ACK, allocate_notification_sequence,
        build_device_info, decode_playback_state, decode_volume_percent, emit_media_info,
        empty_media_info_notification, encode_get_volume_ack, encode_set_volume_ack,
        generate_auth_key, hmac_sha256_hex, parse_json_payload, parse_wfd_url, read_frame,
        run_session, unwrap_payload, volume_notification, wrap_payload, write_frame,
    };
    use crate::MiPlayDeviceType;
    use crate::media::{EventEmitter, StreamKeys};
    use serde_json::Value;
    use std::io::{ErrorKind, Read};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    fn control_notification_sequence() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE + 1))
    }

    #[test]
    fn shutting_down_control_hub_closes_registered_sessions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let generation = hub.media_generation.load(Ordering::Acquire);
        hub.register_session(1, &server).unwrap();
        hub.shutdown_sessions();

        assert!(hub.sessions.lock().unwrap().is_empty());
        assert!(hub.media_generation.load(Ordering::Acquire) > generation);
        let mut byte = [0u8; 1];
        match client.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset
                ) => {}
            result => panic!("MiPlay peer should observe immediate disconnect, got {result:?}"),
        }
    }

    #[test]
    fn receiver_volume_is_sent_to_the_active_source_and_committed() {
        let stored_volume = Arc::new(AtomicU32::new(100));
        let hub = ControlHub::new(Arc::clone(&stored_volume));
        let (sender, receiver) = mpsc::channel();
        let sequence = control_notification_sequence();
        hub.activate(
            7,
            "127.0.0.1".parse().unwrap(),
            sender,
            Arc::clone(&sequence),
        );

        hub.set_volume(42).unwrap();

        let outgoing = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(outgoing.command, NOTIFY);
        assert_eq!(outgoing.sequence, FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE + 1);
        assert_eq!(outgoing.body, volume_notification(42));
        assert_eq!(stored_volume.load(Ordering::Acquire), 42);
    }

    #[test]
    fn reverse_controls_use_mobile_audio_mode_and_unique_sequences() {
        assert_eq!(MOBILE_AUDIO_STREAMING_MODE_ACK, [0, 0, 0, 0, 1]);

        let sequence = AtomicU32::new(FIRST_ENCRYPTED_NOTIFICATION_SEQUENCE);
        assert_eq!(allocate_notification_sequence(&sequence), 8);
        assert_eq!(allocate_notification_sequence(&sequence), 9);
        assert_eq!(allocate_notification_sequence(&sequence), 10);
    }

    #[test]
    fn matches_captured_gen_auth_key() {
        let local: SocketAddr = "192.168.31.128:8899".parse().unwrap();
        let remote: SocketAddr = "192.168.31.207:49668".parse().unwrap();
        assert_eq!(
            generate_auth_key(local, remote),
            "b0175093fe7e51f8ac3cdeb42cd9f513"
        );
    }

    #[test]
    fn wrapper_matches_wire_shape() {
        let body = wrap_payload("cmd", br#"{"ok":true}"#).unwrap();
        let (key, kind, data) = unwrap_payload(&body).unwrap();
        assert_eq!(key, "cmd");
        assert_eq!(kind, 30);
        assert_eq!(data, br#"{"ok":true}"#);
    }

    #[test]
    fn stream_keys_are_shared_between_control_sessions_from_the_same_source() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.220".parse().unwrap();
        let keys =
            StreamKeys::from_strings("1234567890abcdef", "0123456789abcdef", "fedcba9876543210")
                .unwrap();

        hub.remember_stream_keys(source, 41, keys.clone()).unwrap();
        let shared = hub.stream_keys_for(source).unwrap().unwrap();

        assert_eq!(shared.session_id, 41);
        assert_eq!(shared.keys.fingerprint(), keys.fingerprint());
        assert!(
            hub.stream_keys_for("192.168.31.221".parse().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn companion_control_sessions_share_the_active_phone_source() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source: std::net::IpAddr = "192.168.31.207".parse().unwrap();
        let other_source: std::net::IpAddr = "192.168.31.220".parse().unwrap();
        let (outgoing_sender, _outgoing_receiver) = std::sync::mpsc::channel();

        assert_eq!(hub.playback_state_for(42, source), 0);
        let paused = hub.activate(41, source, outgoing_sender, control_notification_sequence());

        assert_eq!(hub.active_source_session_id(41, source), Some(41));
        assert_eq!(hub.active_source_session_id(42, source), Some(41));
        assert_eq!(hub.active_source_session_id(42, other_source), None);
        assert_eq!(hub.playback_state_for(42, source), 2);
        paused.set_paused(true);
        assert_eq!(hub.playback_state_for(42, source), 3);
    }

    #[test]
    fn reverse_controls_use_the_companion_route_that_registered_the_callback() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (media_sender, media_receiver) = std::sync::mpsc::channel();
        hub.activate(41, source, media_sender, control_notification_sequence());

        let (companion_sender, companion_receiver) = std::sync::mpsc::channel();
        hub.register_control_route(42, source, companion_sender, Arc::new(AtomicU32::new(21)));
        hub.mark_play_source_registered(42);
        hub.mark_reverse_control_ready(42);

        hub.send_confirmed(MediaAction::Pause, Duration::ZERO)
            .unwrap();

        assert!(media_receiver.try_recv().is_err());
        let pause = companion_receiver.try_recv().unwrap();
        assert_eq!(pause.command, NOTIFY);
        assert_eq!(pause.sequence, 21);
        assert_eq!(pause.body, b"\x09key-pause\x00\x01");
    }

    #[test]
    fn disconnected_companion_route_falls_back_to_the_media_session() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (media_sender, media_receiver) = std::sync::mpsc::channel();
        hub.activate(41, source, media_sender, control_notification_sequence());

        let (companion_sender, _companion_receiver) = std::sync::mpsc::channel();
        hub.register_control_route(42, source, companion_sender, Arc::new(AtomicU32::new(21)));
        hub.mark_reverse_control_ready(42);
        hub.deactivate(42);

        hub.send_confirmed(MediaAction::Pause, Duration::ZERO)
            .unwrap();

        let pause = media_receiver.try_recv().unwrap();
        assert_eq!(pause.command, NOTIFY);
        assert_eq!(pause.sequence, 9);
        assert_eq!(pause.body, b"\x09key-pause\x00\x01");
    }

    #[test]
    fn restored_paused_snapshot_cannot_mute_an_active_session() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (outgoing_sender, _outgoing_receiver) = std::sync::mpsc::channel();
        let paused = hub.activate(41, source, outgoing_sender, control_notification_sequence());

        assert!(!hub.observe_playback_snapshot(41, true));
        assert!(!paused.is_paused());
        assert_eq!(hub.playback_state_for(41, source), 2);

        assert!(hub.observe_playback(41, true));
        assert!(paused.is_paused());
        assert_eq!(hub.playback_state_for(41, source), 3);

        // A cached playing snapshot can race with the pause in the opposite
        // direction too. It must not reopen the gate during settling.
        assert!(!hub.observe_playback_snapshot(41, false));
        assert!(paused.is_paused());

        assert!(hub.observe_playback(41, false));
        assert!(!paused.is_paused());
    }

    #[test]
    fn replacement_media_session_gets_an_isolated_pause_gate() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (first_sender, _first_receiver) = std::sync::mpsc::channel();
        let first_paused = hub.activate(41, source, first_sender, control_notification_sequence());
        first_paused.set_paused(true);

        let (second_sender, _second_receiver) = std::sync::mpsc::channel();
        let second_paused =
            hub.activate(41, source, second_sender, control_notification_sequence());

        assert!(first_paused.is_paused());
        assert!(!second_paused.is_paused());
        assert_eq!(hub.playback_state_for(41, source), 2);
    }

    #[test]
    fn receiver_output_suspension_survives_media_session_replacement() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (first_sender, _first_receiver) = std::sync::mpsc::channel();
        let first_gate = hub.activate(41, source, first_sender, control_notification_sequence());

        hub.suspend_output();
        assert!(first_gate.is_paused());
        assert!(!first_gate.is_source_paused());

        let (replacement_sender, _replacement_receiver) = std::sync::mpsc::channel();
        let replacement_gate = hub.activate(
            42,
            source,
            replacement_sender,
            control_notification_sequence(),
        );
        replacement_gate.set_paused(false);

        assert!(replacement_gate.is_paused());
        assert!(!replacement_gate.is_source_paused());

        hub.resume_output();
        assert!(!replacement_gate.is_paused());
    }

    #[test]
    fn releasing_output_ownership_preserves_a_source_side_pause() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (sender, _receiver) = std::sync::mpsc::channel();
        let gate = hub.activate(41, source, sender, control_notification_sequence());
        gate.set_paused(true);

        hub.suspend_output();
        hub.resume_output();

        assert!(gate.is_paused());
        assert!(gate.is_source_paused());
    }

    #[test]
    fn media_controls_use_receiver_notify_keys_and_project_dispatched_state() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let (outgoing_sender, outgoing_receiver) = std::sync::mpsc::channel();
        let paused = hub.activate(
            7,
            "192.168.31.207".parse().unwrap(),
            outgoing_sender,
            control_notification_sequence(),
        );

        hub.send_confirmed(MediaAction::Toggle, Duration::ZERO)
            .unwrap();
        let pause = outgoing_receiver.try_recv().unwrap();
        assert_eq!(pause.command, NOTIFY);
        assert_eq!(pause.sequence, 9);
        assert_eq!(pause.body, b"\x09key-pause\x00\x01");
        assert!(paused.is_paused());

        // HyperOS does not always echo PAUSE after applying key-pause. The
        // projected state must therefore make the next toggle send resume;
        // subsequent source frames remain authoritative and can reconcile it.
        hub.send_confirmed(MediaAction::Toggle, Duration::ZERO)
            .unwrap();
        let resume = outgoing_receiver.try_recv().unwrap();
        assert_eq!(resume.command, NOTIFY);
        assert_eq!(resume.sequence, 10);
        assert_eq!(resume.body, b"\x0akey-resume\x00\x01");
        assert!(!paused.is_paused());

        hub.send_confirmed(MediaAction::Seek(123_456), Duration::ZERO)
            .unwrap();
        let seek = outgoing_receiver.try_recv().unwrap();
        assert_eq!(seek.command, NOTIFY);
        assert_eq!(seek.sequence, 11);
        assert_eq!(seek.body, b"\x08key-seek\x09\x00\x00\x00\x00\x00\x01\xe2@");

        hub.send_confirmed(MediaAction::Previous, Duration::ZERO)
            .unwrap();
        let previous = outgoing_receiver.try_recv().unwrap();
        assert_eq!(previous.command, NOTIFY);
        assert_eq!(previous.sequence, 12);
        assert_eq!(previous.body, b"\x08key-prev\x00\x01");

        hub.send_confirmed(MediaAction::Next, Duration::ZERO)
            .unwrap();
        let next = outgoing_receiver.try_recv().unwrap();
        assert_eq!(next.command, NOTIFY);
        assert_eq!(next.sequence, 13);
        assert_eq!(next.body, b"\x08key-next\x00\x01");
    }

    #[test]
    fn rapid_duplicate_pause_is_a_noop_after_the_first_dispatch() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let (outgoing_sender, outgoing_receiver) = std::sync::mpsc::channel();
        hub.activate(
            7,
            "192.168.31.207".parse().unwrap(),
            outgoing_sender,
            control_notification_sequence(),
        );

        let first = hub
            .send_confirmed(MediaAction::Pause, Duration::ZERO)
            .unwrap();
        let second = hub
            .send_confirmed(MediaAction::Pause, Duration::ZERO)
            .unwrap();

        assert_eq!(
            first,
            MediaControlOutcome {
                dispatched: true,
                confirmed: false
            }
        );
        assert_eq!(
            second,
            MediaControlOutcome {
                dispatched: false,
                confirmed: true
            }
        );
        assert_eq!(outgoing_receiver.try_iter().count(), 1);
    }

    #[test]
    fn explicit_pause_is_a_confirmed_noop_after_remote_pause() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let (outgoing_sender, outgoing_receiver) = std::sync::mpsc::channel();
        hub.activate(
            7,
            "192.168.31.207".parse().unwrap(),
            outgoing_sender,
            control_notification_sequence(),
        );
        assert!(hub.observe_playback(7, true));

        let outcome = hub
            .send_confirmed(MediaAction::Pause, Duration::ZERO)
            .unwrap();

        assert_eq!(
            outcome,
            MediaControlOutcome {
                dispatched: false,
                confirmed: true
            }
        );
        assert!(outgoing_receiver.try_recv().is_err());
    }

    #[test]
    fn get_media_info_notification_matches_captured_mipcaudio_media_info_ex() {
        assert_eq!(
            empty_media_info_notification(),
            hex::decode("0b6d65646961496e666f4578160000000c066d5469746c651400000000").unwrap(),
        );
    }

    #[test]
    fn duplicate_position_frames_are_not_republished_to_the_ui() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let (outgoing_sender, _outgoing_receiver) = std::sync::mpsc::channel();
        hub.activate(
            17,
            "192.168.31.207".parse().unwrap(),
            outgoing_sender,
            control_notification_sequence(),
        );

        assert_eq!(hub.observe_position(17, 42_000), (true, false));
        assert_eq!(hub.observe_position(17, 42_000), (false, false));
        assert_eq!(hub.observe_position(17, 42_500), (true, false));
    }

    #[test]
    fn advancing_source_position_cannot_cancel_a_recent_pause() {
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let source = "192.168.31.207".parse().unwrap();
        let (outgoing_sender, _outgoing_receiver) = std::sync::mpsc::channel();
        let paused = hub.activate(17, source, outgoing_sender, control_notification_sequence());
        assert!(hub.observe_playback(17, true));

        assert_eq!(hub.observe_position(17, 42_000), (true, false));
        assert!(paused.is_paused());
        assert_eq!(hub.observe_position(17, 43_000), (true, false));
        assert!(paused.is_paused());
        assert_eq!(hub.playback_state_for(17, source), 3);

        assert!(hub.observe_playback(17, false));
        assert!(!paused.is_paused());
        assert_eq!(hub.playback_state_for(17, source), 2);
    }

    #[test]
    fn device_info_reports_the_selected_device_type() {
        let identity = DeviceIdentity {
            device_id: "A0-36-BC-25-05-43".to_owned(),
            device_name: "FusionPlay".to_owned(),
            model: "Android TV".to_owned(),
            platform: "Android".to_owned(),
            device_type: MiPlayDeviceType::Television,
        };
        let body = build_device_info(&identity);
        let mut entries = std::collections::BTreeMap::new();
        assert_eq!(&body[..3], &[0x00, 0x01, 0x55]);
        let mut offset = 3;
        while offset < body.len() {
            let key_length = usize::from(body[offset]);
            offset += 1;
            let key = String::from_utf8(body[offset..offset + key_length].to_vec()).unwrap();
            offset += key_length;
            assert_eq!(body[offset], 0x0c);
            offset += 1;
            let value_length = u16::from_be_bytes(body[offset..offset + 2].try_into().unwrap());
            offset += 2;
            let value_length = usize::from(value_length);
            let value = String::from_utf8(body[offset..offset + value_length].to_vec()).unwrap();
            offset += value_length;
            entries.insert(key, value);
        }
        assert_eq!(entries["deviceId"], "BC250543");
        assert_eq!(entries["deviceType"], "2");
        assert_eq!(entries["model"], "Android TV");
        assert_eq!(entries["canAlonePlayCtrl"], "0");
        assert_eq!(entries["canHeadsetCtrl"], "0");
        assert_eq!(entries["canRevCtrl"], "1");
        assert_eq!(entries["needLrc"], "1");
        assert_eq!(entries["romVersion"], "");
        assert_eq!(entries["support"], "audio");
        assert_eq!(entries.len(), 13);
    }

    #[test]
    fn control_device_info_preserves_mi_connect_short_id() {
        let identity = DeviceIdentity {
            device_id: "sly".to_owned(),
            device_name: "ASUS".to_owned(),
            model: "Windows PC".to_owned(),
            platform: "Windows".to_owned(),
            device_type: MiPlayDeviceType::Television,
        };
        let body = build_device_info(&identity);

        assert!(
            body.windows(b"deviceId\x0c\x00\x03sly".len())
                .any(|window| { window == b"deviceId\x0c\x00\x03sly" })
        );
        assert!(
            body.windows(b"deviceType\x0c\x00\x012".len())
                .any(|window| { window == b"deviceType\x0c\x00\x012" })
        );
    }

    #[test]
    fn command_session_reports_every_user_selectable_device_type() {
        for device_type in [
            MiPlayDeviceType::Vehicle,
            MiPlayDeviceType::Television,
            MiPlayDeviceType::Tablet,
            MiPlayDeviceType::Speaker,
            MiPlayDeviceType::DisplaySpeaker,
        ] {
            let identity = DeviceIdentity {
                device_id: "sly".to_owned(),
                device_name: "FusionPlay".to_owned(),
                model: device_type.model_name().to_owned(),
                platform: "Android".to_owned(),
                device_type,
            };
            let body = build_device_info(&identity);
            let value = identity.device_type.protocol_value().to_string();
            let mut expected = b"deviceType\x0c".to_vec();
            expected.extend_from_slice(&(value.len() as u16).to_be_bytes());
            expected.extend_from_slice(value.as_bytes());
            assert!(
                body.windows(expected.len())
                    .any(|window| window == expected),
                "command session did not report picker type {device_type:?}",
            );
        }
    }

    #[test]
    fn control_cipher_uses_independent_chained_state() {
        let key = *b"b0175093fe7e51f8";
        let mut sender = ControlCipher::new(key);
        let mut receiver = ControlCipher::new(key);
        for value in [b"first".as_slice(), b"a longer second value".as_slice()] {
            let encrypted = sender.encrypt(value).unwrap();
            assert_eq!(receiver.decrypt(&encrypted).unwrap(), value);
        }
    }

    #[test]
    fn control_header_supports_full_value_type_and_u32_length() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let body = vec![0x5a; 70_000];
        let expected = body.clone();
        let writer = thread::spawn(move || {
            write_frame(&mut server, 0x00, super::SET_POSITION, 0x0100, &body).unwrap();
        });

        let mut cipher = ControlCipher::new(*b"0123456789abcdef");
        let frame = read_frame(&mut client, &mut cipher).unwrap();
        writer.join().unwrap();

        assert_eq!(frame.outer_type, 0x00);
        assert_eq!(frame.command, super::SET_POSITION);
        assert_eq!(frame.sequence, 0x0100);
        assert_eq!(frame.body, expected);
    }

    #[test]
    fn encrypted_auth_frames_match_captured_mipcaudio_bytes() {
        let key = *b"b0175093fe7e51f8";
        let mut cipher = ControlCipher::new(key);
        let auth = b"{\n\t\"authMsg\": \"a36d6fd57705e13323c23db0e8a5347c\" \n} \n";
        let encrypted_auth = cipher.encrypt(&wrap_payload("cmd", auth).unwrap()).unwrap();
        assert_eq!(&encrypted_auth[..5], &[0x00, 0x07, 0x01, 0xe0, 0x02]);
        assert_eq!(&encrypted_auth[5..9], &[0x98, 0xa7, 0xb4, 0xd3]);
        assert_eq!(
            &encrypted_auth[9..],
            hex::decode(concat!(
                "35105d15b265cb733b043fca060ebd32bc490d86f63d3f00",
                "0e225b893c09e9c12af0f54fb686032a931ea9a2c8d09420",
                "b3a2a88a2e767964fef89395ca9b65c7"
            ))
            .unwrap()
        );

        let ack = b"{\n\t\"authMsgAck\": \"a8b39976c4609052116bcda6ace3211beb0cf14f2b19065cac9397d8d9073934\",\n\t\"result\": \"0\" \n} \n";
        let encrypted_ack = cipher.encrypt(&wrap_payload("ack", ack).unwrap()).unwrap();
        assert_eq!(&encrypted_ack[..5], &[0x00, 0x07, 0x01, 0xe0, 0x0f]);
        assert_eq!(&encrypted_ack[5..9], &[0xd8, 0x22, 0x1c, 0x81]);
        assert_eq!(
            &encrypted_ack[9..],
            hex::decode(concat!(
                "9d8ff72441d16e76e8dbde0169ce9f8bd9dcdff2d2ab69",
                "b3a6e78171efbe463aec933bc3ec5334df87c6f634171618",
                "b5947c071ed477350e8779d595db54d845cf141050cfaacd",
                "99c2bf5b8bd220c7cbcb29cf877ef1a49beb89478fd7d2f",
                "d367f038d8cf86228b03b9cd396ca29f0024741916693f89",
                "b176314f89aedb6690a"
            ))
            .unwrap()
        );
    }

    #[test]
    fn mutual_authentication_uses_captured_mipcaudio_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let server_local = server.local_addr().unwrap();
        let client_local = client.local_addr().unwrap();
        let auth_key = generate_auth_key(server_local, client_local);
        let mut key = [0_u8; 16];
        key.copy_from_slice(&auth_key.as_bytes()[..16]);

        let recorded_events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let event_sink = Arc::clone(&recorded_events);
        let events: EventEmitter = Arc::new(move |event| {
            event_sink.lock().unwrap().push(event);
        });
        let hub = ControlHub::new(Arc::new(AtomicU32::new(100)));
        let server_hub = hub.clone();
        let server_events = Arc::clone(&events);
        let server_thread = thread::spawn(move || {
            run_session(
                77,
                server,
                DeviceIdentity {
                    device_id: "A0-36-BC-25-05-43".to_owned(),
                    device_name: "FusionPlay".to_owned(),
                    model: "Windows".to_owned(),
                    platform: "Windows".to_owned(),
                    device_type: MiPlayDeviceType::Television,
                },
                None,
                server_hub,
                server_events,
            )
        });

        let mut server_to_client = ControlCipher::new(key);
        let mut client_to_server = ControlCipher::new(key);

        // The receiver first publishes its plain numeric device id.
        let device_id = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(device_id.command, super::DEVICE_ID);

        let safety_info = concat!(
            "{\n\t\"aesIvTypes\": \"3\",\n\t\"aesKeyTypes\": \"3\",\n",
            "\t\"authAlgorithmTypes\": \"7\",\n\t\"authKeyTypes\": \"1\",\n",
            "\t\"integrityTypes\": \"1\" \n} \n"
        );
        let safety_info = wrap_payload("cmd", safety_info.as_bytes()).unwrap();
        write_frame(&mut client, 0x14, SAFETY_INFO, 1, &safety_info).unwrap();

        // MiPCAudio sends SafetyInfoAck followed immediately by its challenge.
        let safety_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(safety_ack.command, SAFETY_INFO_ACK);
        let server_challenge = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(server_challenge.command, SAFETY_AUTH);
        let server_auth_message = parse_json_payload(&server_challenge.body)
            .unwrap()
            .get("authMsg")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();

        let client_auth_message = "0123456789abcdeffedcba9876543210";
        let client_challenge = format!("{{\n\t\"authMsg\": \"{client_auth_message}\" \n}} \n");
        let client_challenge = client_to_server
            .encrypt(&wrap_payload("cmd", client_challenge.as_bytes()).unwrap())
            .unwrap();
        write_frame(&mut client, 0x14, SAFETY_AUTH, 2, &client_challenge).unwrap();

        let server_auth_ack = hmac_sha256_hex(auth_key.as_bytes(), server_auth_message.as_bytes());
        let client_ack =
            format!("{{\n\t\"authMsgAck\": \"{server_auth_ack}\",\n\t\"result\": \"0\" \n}} \n");
        let client_ack = client_to_server
            .encrypt(&wrap_payload("ack", client_ack.as_bytes()).unwrap())
            .unwrap();
        write_frame(&mut client, 0x14, SAFETY_AUTH_ACK, 0, &client_ack).unwrap();

        // Only after verifying the client's ack may the receiver acknowledge
        // the client's challenge.
        let final_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(final_ack.command, SAFETY_AUTH_ACK);
        assert_eq!(final_ack.sequence, 2);
        let received_ack = parse_json_payload(&final_ack.body)
            .unwrap()
            .get("authMsgAck")
            .and_then(Value::as_str)
            .unwrap()
            .to_owned();
        assert_eq!(
            received_ack,
            hmac_sha256_hex(auth_key.as_bytes(), client_auth_message.as_bytes())
        );

        // The authenticated identity directly preserves the selected picker
        // type; no temporary computer identity or follow-up rewrite is used.
        let get_device_info = client_to_server.encrypt(&[]).unwrap();
        write_frame(
            &mut client,
            0x00,
            super::GET_DEVICE_INFO,
            3,
            &get_device_info,
        )
        .unwrap();
        let device_info = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(device_info.command, super::GET_DEVICE_INFO_ACK);
        assert!(
            device_info
                .body
                .windows(b"deviceType\x0c\x00\x012".len())
                .any(|window| window == b"deviceType\x0c\x00\x012")
        );

        let get_mirror_mode = client_to_server.encrypt(&[]).unwrap();
        write_frame(
            &mut client,
            0x00,
            super::GET_MIRROR_MODE,
            4,
            &get_mirror_mode,
        )
        .unwrap();
        let mirror_mode = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(mirror_mode.command, super::GET_MIRROR_MODE_ACK);
        // HyperOS probes the receiver state before asking for media metadata.
        // MiPCAudio reports success plus the idle state while no stream has
        // been opened yet.
        let get_state = client_to_server.encrypt(&[]).unwrap();
        write_frame(&mut client, 0x00, super::GET_STATE, 9, &get_state).unwrap();
        let state_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(state_ack.command, super::GET_STATE_ACK);
        assert_eq!(state_ack.sequence, 9);
        assert_eq!(state_ack.body, vec![0, 0, 0, 0, 0]);

        // The source keeps this same reverse-control route alive with 0x1a.
        // MiPCAudio replies immediately using 0x1b and the same sequence.
        let heartbeat = client_to_server.encrypt(&[]).unwrap();
        write_frame(&mut client, 0x00, super::HEART_BEAT, 10, &heartbeat).unwrap();
        let heartbeat_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(heartbeat_ack.command, super::HEART_BEAT_ACK);
        assert_eq!(heartbeat_ack.sequence, 10);
        assert!(heartbeat_ack.body.is_empty());

        // HyperOS also probes the TV standalone-player namespace on this
        // companion route. The response must preserve the 0x04 high byte;
        // replying as ordinary command 0x0017 leaves reverse controls stuck.
        let alone_get_state = client_to_server.encrypt(br#"{"getState":"1"}"#).unwrap();
        write_frame(
            &mut client,
            super::ALONE_MEDIA_PLAYER_NAMESPACE,
            super::ALONE_MEDIA_PLAYER_GET_STATE,
            11,
            &alone_get_state,
        )
        .unwrap();
        let alone_state_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(
            alone_state_ack.outer_type,
            super::ALONE_MEDIA_PLAYER_NAMESPACE
        );
        assert_eq!(
            alone_state_ack.command,
            super::ALONE_MEDIA_PLAYER_GET_STATE_ACK
        );
        assert_eq!(alone_state_ack.sequence, 11);
        assert_eq!(alone_state_ack.body, vec![0, 0, 0, 0, 0]);

        // Control Center registers its actual source before accepting
        // receiver-originated key commands.
        let play_source = client_to_server
            .encrypt(
                br#"{"ref_channel":"controlcenter","ref_function":"single_room","ref_content":"music_qq"}"#,
            )
            .unwrap();
        write_frame(&mut client, 0x00, super::SET_PLAY_SOURCE, 12, &play_source).unwrap();
        let play_source_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(play_source_ack.outer_type, 0x00);
        assert_eq!(play_source_ack.command, super::SET_PLAY_SOURCE_ACK);
        assert_eq!(play_source_ack.sequence, 12);
        assert!(play_source_ack.body.is_empty());

        // Current HyperOS requests media info before OPEN. MiPCAudio answers
        // with the first encrypted NOTIFY (value type 9), not a 0x15 ACK.
        // This exchange installs the reverse-control callback on the phone.
        let get_media_info = client_to_server.encrypt(&[]).unwrap();
        write_frame(&mut client, 0x00, GET_MEDIA_INFO, 13, &get_media_info).unwrap();
        let media_info_notification = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(media_info_notification.command, NOTIFY);
        assert_eq!(media_info_notification.sequence, 8);
        assert_eq!(
            media_info_notification.body,
            empty_media_info_notification(),
        );

        // SET_VOLUME is acknowledged as a status-only command. The applied
        // value is then published through NOTIFY so HyperOS updates the route
        // cache that backs its volume slider instead of restoring a stale value.
        let set_volume = client_to_server.encrypt(&42_i32.to_be_bytes()).unwrap();
        write_frame(&mut client, 0x00, super::SET_VOLUME, 14, &set_volume).unwrap();
        let set_volume_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(set_volume_ack.command, super::SET_VOLUME_ACK);
        assert_eq!(set_volume_ack.sequence, 14);
        assert!(set_volume_ack.body.is_empty());
        let volume_notify = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(volume_notify.command, NOTIFY);
        assert_eq!(volume_notify.sequence, 9);
        assert_eq!(volume_notify.body, volume_notification(42));

        // The committed value must also be returned by every later query.
        let get_volume = client_to_server.encrypt(&[]).unwrap();
        write_frame(&mut client, 0x00, super::GET_VOLUME, 15, &get_volume).unwrap();
        let get_volume_ack = read_frame(&mut client, &mut server_to_client).unwrap();
        assert_eq!(get_volume_ack.command, super::GET_VOLUME_ACK);
        assert_eq!(get_volume_ack.sequence, 15);
        assert_eq!(get_volume_ack.body, encode_get_volume_ack(42));

        thread::sleep(Duration::from_millis(25));
        assert!(recorded_events.lock().unwrap().iter().any(|event| {
            event.get("stage").and_then(Value::as_str) == Some("secure_channel_established")
        }));

        // A compatibility probe is not a playable route. Controls become
        // available only after the source sends an encrypted OPEN command.
        assert!(hub.send(super::MediaAction::Pause).is_err());

        drop(client);
        hub.deactivate(77);
        assert!(server_thread.join().unwrap().is_err());
    }

    #[test]
    fn parses_wfd_url() {
        assert_eq!(
            parse_wfd_url("wfd://192.168.31.207:39119?mirrorMode=1").unwrap(),
            ("192.168.31.207".parse().unwrap(), 39119),
        );
    }

    #[test]
    fn media_info_has_stable_track_identity_and_cover_url_fallback() {
        let recorded = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&recorded);
        let events: EventEmitter = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        let body_without_duration = br#"{
            "mTitle":"Track B",
            "mArtist":"Artist",
            "mAlbum":"Album",
            "id":"",
            "mArt":"",
            "mCoverUrl":"https://example.invalid/cover.jpg"
        }"#;
        let body_with_duration = br#"{
            "mTitle":"Track B",
            "mArtist":"Artist",
            "mAlbum":"Album",
            "mDuration":180000,
            "id":"",
            "mArt":"",
            "mCoverUrl":"https://example.invalid/cover.jpg"
        }"#;

        emit_media_info(body_without_duration, &events);
        emit_media_info(body_with_duration, &events);

        let events = recorded.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "media_info");
        assert_eq!(events[0]["artwork"], "https://example.invalid/cover.jpg");
        assert!(
            events[0]["track_id"]
                .as_str()
                .unwrap()
                .starts_with("metadata:")
        );
        assert_eq!(events[0]["track_id"], events[1]["track_id"]);
        assert_eq!(events[1]["duration_ms"], 180_000);
    }

    #[test]
    fn media_info_accepts_xiaomi_metadata_model_fields() {
        let recorded = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&recorded);
        let events: EventEmitter = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        emit_media_info(
            br#"{
                "mTitle":"Track E",
                "mArtist":"Artist E",
                "mDuration":240000,
                "mPosition":42000,
                "mAudioId":"audio-e",
                "mMetaChangeType":2
            }"#,
            &events,
        );

        let events = recorded.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["track_id"], "audio-e");
        assert_eq!(events[0]["title"], "Track E");
        assert_eq!(events[0]["artist"], "Artist E");
        assert_eq!(events[0]["duration_ms"], 240_000);
        assert_eq!(events[0]["position_ms"], 42_000);
        assert_eq!(events[0]["metadata_change_type"], 2);
    }

    #[test]
    fn volume_payload_is_big_endian_and_clamped() {
        assert_eq!(decode_volume_percent(&27_i32.to_be_bytes()), 27);
        assert_eq!(decode_volume_percent(&150_i32.to_be_bytes()), 100);
        assert_eq!(decode_volume_percent(&(-1_i32).to_be_bytes()), 0);
        assert_eq!(decode_volume_percent(&[0, 1]), 100);
    }

    #[test]
    fn get_volume_ack_has_status_byte_and_clamped_big_endian_value() {
        assert_eq!(encode_get_volume_ack(0), [0, 0, 0, 0, 0]);
        assert_eq!(encode_get_volume_ack(54), [0, 0, 0, 0, 54]);
        assert_eq!(encode_get_volume_ack(150), [0, 0, 0, 0, 100]);
    }

    #[test]
    fn set_volume_ack_is_status_only_and_has_no_body() {
        assert!(encode_set_volume_ack().is_empty());
    }

    #[test]
    fn volume_notification_matches_reference_receiver_wire_format() {
        assert_eq!(
            volume_notification(55),
            hex::decode("06766f6c756d650700000037").unwrap(),
        );
        assert_eq!(
            volume_notification(150),
            hex::decode("06766f6c756d650700000064").unwrap(),
        );
    }

    #[test]
    fn playback_state_accepts_standard_and_standalone_payloads() {
        assert_eq!(decode_playback_state(&2_u32.to_be_bytes()), Some(true));
        assert_eq!(decode_playback_state(&3_u32.to_be_bytes()), Some(false));
        assert_eq!(decode_playback_state(br#"{"setState":"2"}"#), Some(true));
        assert_eq!(decode_playback_state(br#"{"state":3}"#), Some(false));
        assert_eq!(decode_playback_state(br#"{"state":0}"#), None);
    }

    #[test]
    fn media_info_volume_placeholder_is_ignored() {
        let recorded = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&recorded);
        let events: EventEmitter = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        emit_media_info(br#"{"mTitle":"Track","volume":0}"#, &events);

        assert!(
            recorded
                .lock()
                .unwrap()
                .iter()
                .all(|event| event["event"] != "volume")
        );
    }

    #[test]
    fn media_info_accepts_sdk_aliases_for_track_changes() {
        let recorded = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&recorded);
        let events: EventEmitter = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        emit_media_info(
            br#"{
                "title":"Track C",
                "artist":"Artist C",
                "album":"Album C",
                "durationMs":"240000",
                "mediaId":"track-c",
                "artworkUrl":"https://example.invalid/c.jpg"
            }"#,
            &events,
        );

        let events = recorded.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["track_id"], "track-c");
        assert_eq!(events[0]["title"], "Track C");
        assert_eq!(events[0]["artist"], "Artist C");
        assert_eq!(events[0]["album"], "Album C");
        assert_eq!(events[0]["duration_ms"], 240_000);
        assert_eq!(events[0]["artwork"], "https://example.invalid/c.jpg");
    }

    #[test]
    fn media_info_accepts_standalone_nested_json() {
        let recorded = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = Arc::clone(&recorded);
        let events: EventEmitter = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        emit_media_info(
            br#"{"mediaInfo":{"mTitle":"Track D","mArtist":"Artist D","id":"track-d","mArt":"https://example.invalid/d.jpg"}}"#,
            &events,
        );

        let events = recorded.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["track_id"], "track-d");
        assert_eq!(events[0]["title"], "Track D");
        assert_eq!(events[0]["artist"], "Artist D");
        assert_eq!(events[0]["artwork"], "https://example.invalid/d.jpg");
    }
}
