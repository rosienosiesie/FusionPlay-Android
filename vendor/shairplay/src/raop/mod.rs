//! RAOP/AirPlay server core — connection handling, audio pipeline, and public API.

pub use crate::proto::dmap::TrackMetadata;

#[cfg(feature = "ap2")]
pub mod audio_pipeline;
pub mod buffer;
#[cfg(feature = "ap2")]
pub mod buffered_audio;
#[cfg(feature = "ap2")]
pub(crate) mod event_channel;
pub(crate) mod handlers_ap1;
#[cfg(feature = "ap2")]
pub(crate) mod handlers_ap2;
#[cfg(feature = "hls")]
pub(crate) mod handlers_hls;
#[cfg(feature = "hls")]
pub mod hls;
pub(crate) mod ntp;
#[cfg(feature = "ap2")]
pub(crate) mod realtime_audio;
pub(crate) mod rtp;
mod rtsp;
#[cfg(feature = "video")]
pub mod video;
#[cfg(feature = "video")]
pub(crate) mod video_stream;

pub(crate) mod config;

/// Maximum hardware address length in bytes.
pub(crate) const MAX_HWADDR_LEN: usize = 6;
/// Maximum password length in bytes.
pub(crate) const MAX_PASSWORD_LEN: usize = 64;
/// Maximum HTTP Digest nonce length in bytes.
pub(crate) const MAX_NONCE_LEN: usize = 32;

mod types;
pub use types::*;

mod connection;
mod server;
pub use server::{RaopMediaControl, RaopServer, RaopServerBuilder};

pub(crate) struct DacpRemoteControl {
    client: crate::dacp::DacpClient,
}

impl DacpRemoteControl {
    /// Create a new DACP remote control client for the given iPhone.
    pub(crate) fn new(dacp_id: &str, active_remote: &str, remote_addr: &[u8]) -> Self {
        let mut client = crate::dacp::DacpClient::new(dacp_id, active_remote);
        let ip = match remote_addr.len() {
            4 => std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                remote_addr[0],
                remote_addr[1],
                remote_addr[2],
                remote_addr[3],
            )),
            16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(remote_addr);
                std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets))
            }
            _ => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        };
        client.discover_from_remote(ip);
        Self { client }
    }
}

impl RemoteControl for DacpRemoteControl {
    fn send_command(&self, cmd: RemoteCommand) -> Result<(), crate::error::ShairplayError> {
        let result = match cmd {
            RemoteCommand::Play => self.client.play_blocking(),
            RemoteCommand::Pause => self.client.pause_blocking(),
            RemoteCommand::PlayPause => self.client.play_pause_blocking(),
            RemoteCommand::NextTrack => self.client.next_blocking(),
            RemoteCommand::PreviousTrack => self.client.prev_blocking(),
            RemoteCommand::SetVolume(v) => self.client.set_volume_blocking(v),
            RemoteCommand::ToggleShuffle => self.client.set_shuffle_blocking(true),
            RemoteCommand::ToggleRepeat => self.client.set_repeat_blocking(1),
            RemoteCommand::SeekToPosition(position_ms) => self.client.seek_blocking(position_ms),
            RemoteCommand::Stop => self.client.stop_blocking(),
        };
        result.map_err(crate::error::ShairplayError::Network)
    }

    fn available_commands(&self) -> Vec<RemoteCommand> {
        vec![
            RemoteCommand::Play,
            RemoteCommand::Pause,
            RemoteCommand::PlayPause,
            RemoteCommand::NextTrack,
            RemoteCommand::PreviousTrack,
            RemoteCommand::SetVolume(0),
            RemoteCommand::ToggleShuffle,
            RemoteCommand::ToggleRepeat,
            RemoteCommand::SeekToPosition(0),
            RemoteCommand::Stop,
        ]
    }

    fn transport_name(&self) -> &'static str {
        "dacp"
    }
}

#[cfg(feature = "ap2")]
const PLAYBACK_STATE_RECONCILIATION_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(feature = "ap2")]
#[derive(Debug, Clone, Copy)]
struct PendingPlaybackState {
    rate: u32,
    issued_at: std::time::Instant,
    deferred_rate: Option<u32>,
}

#[cfg(feature = "ap2")]
#[derive(Debug)]
struct PlaybackStateProjection {
    rate: u32,
    pending: Option<PendingPlaybackState>,
}

#[cfg(feature = "ap2")]
pub(crate) struct Ap2RemoteControl {
    sender: crate::raop::event_channel::EventSender,
    destination_device_uid: String,
    receiver_name: String,
    available_commands: std::sync::RwLock<Vec<RemoteCommand>>,
    playback: std::sync::Mutex<PlaybackStateProjection>,
    cseq: std::sync::atomic::AtomicU32,
}

#[cfg(feature = "ap2")]
impl Ap2RemoteControl {
    pub(crate) fn new(
        sender: crate::raop::event_channel::EventSender,
        destination_device_uid: String,
        receiver_name: String,
    ) -> Self {
        Self {
            sender,
            destination_device_uid,
            receiver_name,
            available_commands: std::sync::RwLock::new(Vec::new()),
            playback: std::sync::Mutex::new(PlaybackStateProjection {
                rate: u32::MAX,
                pending: None,
            }),
            cseq: std::sync::atomic::AtomicU32::new(1),
        }
    }

    pub(crate) fn update_available_commands(&self, commands: Vec<RemoteCommand>) {
        if let Ok(mut available) = self.available_commands.write() {
            *available = commands;
        }
    }

    /// Reconciles a sender-originated playback report with the newest command
    /// queued by the receiver.
    ///
    /// MediaRemote commands and RTSP rate updates travel over separate TCP
    /// channels. During rapid play/pause input, the acknowledgement for an
    /// older command can therefore arrive after a newer command was queued.
    /// Keep the latest receiver intent authoritative for a short bounded
    /// window, while still accepting matching reports and all later sender
    /// changes once the window has elapsed.
    pub(crate) fn update_playback_rate(&self, rate: u32) -> bool {
        self.update_playback_rate_at(rate, std::time::Instant::now())
    }

    fn update_playback_rate_at(&self, rate: u32, now: std::time::Instant) -> bool {
        let normalized = u32::from(rate != 0);
        let Ok(mut playback) = self.playback.lock() else {
            return true;
        };
        if let Some(pending) = playback.pending {
            let within_reconciliation_window = now
                .checked_duration_since(pending.issued_at)
                .is_some_and(|elapsed| elapsed < PLAYBACK_STATE_RECONCILIATION_WINDOW);
            if within_reconciliation_window {
                if normalized != pending.rate {
                    playback.pending = Some(PendingPlaybackState {
                        deferred_rate: Some(normalized),
                        ..pending
                    });
                    return false;
                }
                playback.rate = normalized;
                playback.pending = Some(PendingPlaybackState {
                    deferred_rate: None,
                    ..pending
                });
                return true;
            }
            playback.pending = None;
        }
        playback.rate = normalized;
        true
    }

    fn apply_successful_command_state(
        playback: &mut PlaybackStateProjection,
        command: &RemoteCommand,
        now: std::time::Instant,
    ) {
        let next_rate = match command {
            RemoteCommand::Play => Some(1),
            RemoteCommand::Pause | RemoteCommand::Stop => Some(0),
            RemoteCommand::PlayPause => Some(if playback.rate == 0 { 1 } else { 0 }),
            _ => None,
        };
        if let Some(rate) = next_rate {
            playback.rate = rate;
            playback.pending = Some(PendingPlaybackState {
                rate,
                issued_at: now,
                deferred_rate: None,
            });
        }
    }

    fn expire_pending_state(playback: &mut PlaybackStateProjection, now: std::time::Instant) {
        let Some(pending) = playback.pending else {
            return;
        };
        let expired = now
            .checked_duration_since(pending.issued_at)
            .is_some_and(|elapsed| elapsed >= PLAYBACK_STATE_RECONCILIATION_WINDOW);
        if expired {
            if let Some(deferred_rate) = pending.deferred_rate {
                playback.rate = deferred_rate;
            }
            playback.pending = None;
        }
    }

    fn resolve_command(&self, requested: RemoteCommand, playback_rate: u32) -> Option<RemoteCommand> {
        let available = self.available_commands.read().ok()?;
        // Prefer the explicit command that matches the sender's current state.
        // Some Apple Music versions advertise TogglePlayPause but ignore it on
        // redirected AirPlay routes while accepting Play/Pause.
        if requested == RemoteCommand::PlayPause {
            if playback_rate == 0 && available.contains(&RemoteCommand::Play) {
                return Some(RemoteCommand::Play);
            }
            if playback_rate != 0 && playback_rate != u32::MAX && available.contains(&RemoteCommand::Pause) {
                return Some(RemoteCommand::Pause);
            }
            if available.contains(&RemoteCommand::PlayPause) {
                return Some(RemoteCommand::PlayPause);
            }

            let has_play = available.contains(&RemoteCommand::Play);
            let has_pause = available.contains(&RemoteCommand::Pause);
            return match (has_play, has_pause) {
                (true, false) => Some(RemoteCommand::Play),
                (false, true) => Some(RemoteCommand::Pause),
                // Before the first rate callback, an active receiver control
                // is necessarily acting on a playing RECORD session. Choosing
                // the explicitly advertised Pause command is both safe and
                // avoids rejecting a valid control as "not advertised".
                (true, true) => Some(RemoteCommand::Pause),
                _ => None,
            };
        }

        if available.contains(&requested) {
            return Some(requested);
        }

        // Explicit UI/SMTC Play and Pause can still use the toggle command if
        // that is the only control advertised by the sender.
        if matches!(requested, RemoteCommand::Play | RemoteCommand::Pause)
            && available.contains(&RemoteCommand::PlayPause)
        {
            return Some(RemoteCommand::PlayPause);
        }

        if let RemoteCommand::SeekToPosition(position_ms) = requested
            && available
                .iter()
                .any(|command| matches!(command, RemoteCommand::SeekToPosition(_)))
        {
            return Some(RemoteCommand::SeekToPosition(position_ms));
        }

        None
    }

    fn destination_device_uids(&self) -> Result<Vec<u8>, crate::error::ShairplayError> {
        let mut root = plist::Dictionary::new();
        root.insert("$version".into(), plist::Value::Integer(100_000.into()));
        root.insert("$archiver".into(), plist::Value::String("NSKeyedArchiver".into()));

        let mut top = plist::Dictionary::new();
        top.insert("root".into(), plist::Value::Uid(plist::Uid::new(1)));
        root.insert("$top".into(), plist::Value::Dictionary(top));

        let mut array_object = plist::Dictionary::new();
        array_object.insert(
            "NS.objects".into(),
            plist::Value::Array(vec![plist::Value::Uid(plist::Uid::new(2))]),
        );
        array_object.insert("$class".into(), plist::Value::Uid(plist::Uid::new(3)));

        let mut class_object = plist::Dictionary::new();
        class_object.insert("$classname".into(), plist::Value::String("NSMutableArray".into()));
        class_object.insert(
            "$classes".into(),
            plist::Value::Array(vec![
                plist::Value::String("NSMutableArray".into()),
                plist::Value::String("NSArray".into()),
                plist::Value::String("NSObject".into()),
            ]),
        );

        root.insert(
            "$objects".into(),
            plist::Value::Array(vec![
                plist::Value::String("$null".into()),
                plist::Value::Dictionary(array_object),
                plist::Value::String(self.destination_device_uid.clone()),
                plist::Value::Dictionary(class_object),
            ]),
        );

        let mut archive = Vec::new();
        plist::to_writer_binary(&mut archive, &root).map_err(|error| {
            crate::error::ShairplayError::Protocol(crate::error::ProtocolError::Plist(error.to_string()))
        })?;
        Ok(archive)
    }

    fn build_command_message(&self, command: &RemoteCommand) -> Result<Vec<u8>, crate::error::ShairplayError> {
        let (command_id, legacy_value) = match command {
            RemoteCommand::Play => (0_i64, "play"),
            RemoteCommand::Pause => (1_i64, "paus"),
            RemoteCommand::PlayPause => (2_i64, "plps"),
            RemoteCommand::NextTrack => (4, "next"),
            RemoteCommand::PreviousTrack => (5, "prev"),
            RemoteCommand::ToggleShuffle => (6, "shuf"),
            RemoteCommand::ToggleRepeat => (7, "rept"),
            RemoteCommand::SeekToPosition(_) => (24, "seek"),
            RemoteCommand::Stop => (3, "stop"),
            RemoteCommand::SetVolume(_) => {
                return Err(crate::error::ShairplayError::Protocol(
                    crate::error::ProtocolError::InvalidRtsp(
                        "AirPlay 2 MediaRemote volume commands are not implemented".into(),
                    ),
                ));
            }
        };

        let mut params = plist::Dictionary::new();
        params.insert(
            "kMRMediaRemoteOptionIsRedirectingCommand".into(),
            plist::Value::Integer(1.into()),
        );
        params.insert(
            "kMRMediaRemoteOptionSendOptionsNumber".into(),
            plist::Value::Integer(0.into()),
        );
        params.insert(
            "kMRMediaRemoteOptionDestinationDeviceUIDs".into(),
            plist::Value::Data(self.destination_device_uids()?),
        );
        params.insert(
            "kMRMediaRemoteOptionOriginatedFromRemoteDevice".into(),
            plist::Value::Integer(1.into()),
        );
        if let RemoteCommand::SeekToPosition(position_ms) = command {
            params.insert(
                "kMRMediaRemoteOptionPlaybackPosition".into(),
                plist::Value::Real(*position_ms as f64 / 1000.0),
            );
        }
        params.insert(
            "kMRMediaRemoteOptionCommandID".into(),
            plist::Value::String(uuid::Uuid::new_v4().to_string().to_uppercase()),
        );
        params.insert(
            "kMRMediaRemoteOptionSenderID".into(),
            plist::Value::String(format!(
                "SenderDevice = <{}>, SenderBundleIdentifier = <airplay-receiver>, SenderPID = <0>",
                self.receiver_name
            )),
        );

        let mut command_plist = plist::Dictionary::new();
        command_plist.insert(
            "modernMediaRemoteCommand".into(),
            plist::Value::String(command_id.to_string()),
        );
        command_plist.insert("value".into(), plist::Value::String(legacy_value.to_owned()));
        command_plist.insert("type".into(), plist::Value::String("sendMediaRemoteCommand".into()));
        command_plist.insert("params".into(), plist::Value::Dictionary(params));

        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &command_plist).map_err(|error| {
            crate::error::ShairplayError::Protocol(crate::error::ProtocolError::Plist(error.to_string()))
        })?;
        let cseq = self.cseq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let header = format!(
            "POST /command RTSP/1.0\r\nContent-Length: {}\r\nContent-Type: application/x-apple-binary-plist\r\nCSeq: {cseq}\r\n\r\n",
            body.len()
        );
        let mut message = header.into_bytes();
        message.extend_from_slice(&body);
        Ok(message)
    }
}

#[cfg(feature = "ap2")]
impl RemoteControl for Ap2RemoteControl {
    fn send_command(&self, command: RemoteCommand) -> Result<(), crate::error::ShairplayError> {
        // Hold the projection lock until the command has been queued and its
        // desired state recorded. A sender callback racing on the RTSP channel
        // can then only observe either the complete old state or the complete
        // new state, never the gap between queueing and projection.
        let mut playback = self.playback.lock().map_err(|_| {
            crate::error::ShairplayError::Protocol(crate::error::ProtocolError::InvalidRtsp(
                "the AirPlay 2 playback projection is unavailable".into(),
            ))
        })?;
        let now = std::time::Instant::now();
        Self::expire_pending_state(&mut playback, now);
        let Some(command) = self.resolve_command(command, playback.rate) else {
            return Err(crate::error::ShairplayError::Protocol(
                crate::error::ProtocolError::InvalidRtsp(
                    "the AirPlay 2 sender did not advertise this MediaRemote command".into(),
                ),
            ));
        };
        self.sender
            .send(self.build_command_message(&command)?)
            .map_err(crate::error::ShairplayError::Network)?;
        // MediaRemote does not acknowledge command execution on this channel.
        // Keep the local resolver aligned with the command successfully queued
        // to the sender so a second PlayPause reverses the first one instead of
        // issuing Pause twice after a stream-only teardown.
        Self::apply_successful_command_state(&mut playback, &command, now);
        Ok(())
    }

    fn available_commands(&self) -> Vec<RemoteCommand> {
        self.available_commands
            .read()
            .map(|available| available.clone())
            .unwrap_or_default()
    }

    fn transport_name(&self) -> &'static str {
        "airplay2_mediaremote_experimental"
    }
}

#[cfg(all(test, feature = "ap2"))]
mod ap2_remote_control_tests {
    use super::{Ap2RemoteControl, RemoteCommand, RemoteControl};

    fn decode_command(message: &[u8]) -> plist::Dictionary {
        let body_offset = message
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| offset + 4)
            .unwrap();
        plist::from_bytes::<plist::Value>(&message[body_offset..])
            .unwrap()
            .into_dictionary()
            .unwrap()
    }

    #[test]
    fn play_pause_uses_media_remote_toggle_command() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::PlayPause]);

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("2")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("plps"));
    }

    #[test]
    fn play_pause_falls_back_to_pause_when_sender_is_playing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::Play, RemoteCommand::Pause]);
        remote.update_playback_rate(1);

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("1")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("paus"));

        // A pause may tear down only the sender's realtime audio stream before
        // it publishes another playback-rate callback. The next toggle must
        // therefore reverse the command we just queued instead of pausing a
        // second time.
        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());
        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("0")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("play"));
    }

    #[test]
    fn delayed_sender_state_cannot_reverse_the_latest_rapid_command() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Android".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::Play, RemoteCommand::Pause]);
        assert!(remote.update_playback_rate(1));

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        assert_eq!(
            decode_command(&rx.try_recv().unwrap())
                .get("value")
                .and_then(plist::Value::as_string),
            Some("paus")
        );
        remote.send_command(RemoteCommand::PlayPause).unwrap();
        assert_eq!(
            decode_command(&rx.try_recv().unwrap())
                .get("value")
                .and_then(plist::Value::as_string),
            Some("play")
        );

        // The pause report belongs to the first command and arrives on a
        // different AirPlay TCP channel after the newer play was queued.
        assert!(!remote.update_playback_rate(0));
        assert!(remote.update_playback_rate(1));
        // Keep the reconciliation fence after the matching report as another
        // delayed pause can still be waiting behind it.
        assert!(!remote.update_playback_rate(0));

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        assert_eq!(
            decode_command(&rx.try_recv().unwrap())
                .get("value")
                .and_then(plist::Value::as_string),
            Some("paus")
        );
    }

    #[test]
    fn play_pause_falls_back_to_play_when_sender_is_paused() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::Play]);
        remote.update_playback_rate(0);

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("0")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("play"));
    }

    #[test]
    fn play_pause_uses_advertised_pause_before_first_rate_update() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Android".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::Play, RemoteCommand::Pause]);

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("1")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("paus"));
    }

    #[test]
    fn explicit_pause_is_preferred_when_sender_advertises_all_playback_commands() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![
            RemoteCommand::Play,
            RemoteCommand::Pause,
            RemoteCommand::PlayPause,
        ]);
        remote.update_playback_rate(1);

        remote.send_command(RemoteCommand::PlayPause).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("1")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("paus"));
    }

    #[test]
    fn seek_encodes_absolute_position_in_seconds() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::SeekToPosition(0)]);

        remote.send_command(RemoteCommand::SeekToPosition(91_250)).unwrap();
        let command = decode_command(&rx.try_recv().unwrap());

        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("24")
        );
        assert_eq!(
            command
                .get("params")
                .and_then(plist::Value::as_dictionary)
                .and_then(|params| params.get("kMRMediaRemoteOptionPlaybackPosition"))
                .and_then(plist::Value::as_real),
            Some(91.25)
        );
    }

    #[test]
    fn media_remote_command_is_queued_as_binary_plist_rtsp() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Ap2RemoteControl::new(
            crate::raop::event_channel::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        );
        remote.update_available_commands(vec![RemoteCommand::NextTrack]);

        remote.send_command(RemoteCommand::NextTrack).unwrap();
        let message = rx.try_recv().unwrap();
        let body_offset = message
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| offset + 4)
            .unwrap();
        let header = std::str::from_utf8(&message[..body_offset]).unwrap();
        assert!(header.starts_with("POST /command RTSP/1.0\r\n"));
        assert!(header.contains("\r\nCSeq: 1\r\n"));
        assert!(header.contains(&format!("\r\nContent-Length: {}\r\n", message.len() - body_offset)));
        let command: plist::Value = plist::from_bytes(&message[body_offset..]).unwrap();
        let command = command.as_dictionary().unwrap();

        assert_eq!(
            command.get("type").and_then(plist::Value::as_string),
            Some("sendMediaRemoteCommand")
        );
        assert_eq!(
            command
                .get("modernMediaRemoteCommand")
                .and_then(plist::Value::as_string),
            Some("4")
        );
        assert_eq!(command.get("value").and_then(plist::Value::as_string), Some("next"));

        let destination_archive = command
            .get("params")
            .and_then(plist::Value::as_dictionary)
            .and_then(|params| params.get("kMRMediaRemoteOptionDestinationDeviceUIDs"))
            .and_then(plist::Value::as_data)
            .unwrap();
        let destination: plist::Value = plist::from_bytes(destination_archive).unwrap();
        let objects = destination
            .as_dictionary()
            .and_then(|archive| archive.get("$objects"))
            .and_then(plist::Value::as_array)
            .unwrap();
        assert!(
            objects
                .iter()
                .any(|object| { object.as_string() == Some("3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD") })
        );

        remote.send_command(RemoteCommand::NextTrack).unwrap();
        let second_message = rx.try_recv().unwrap();
        let second_header_end = second_message
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| offset + 4)
            .unwrap();
        let second_header = std::str::from_utf8(&second_message[..second_header_end]).unwrap();
        assert!(second_header.contains("\r\nCSeq: 2\r\n"));
    }
}
