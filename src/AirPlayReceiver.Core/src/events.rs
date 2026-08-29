use serde::Serialize;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent<'a> {
    Status {
        state: &'a str,
        message: &'a str,
    },
    ReceiverReady {
        name: &'a str,
        pin: Option<&'a str>,
        port: u16,
        device_id: &'a str,
    },
    OutputDevice {
        name: &'a str,
        id: &'a str,
        is_default: bool,
        sample_rate: u32,
        channels: u16,
        sample_format: &'a str,
        bits_per_sample: u8,
    },
    ClientConnected {
        address: &'a str,
    },
    ClientDisconnected {
        address: &'a str,
    },
    StreamStarted {
        source: &'a str,
        epoch: u64,
        source_codec: Option<&'a str>,
        source_sample_rate: Option<u32>,
        source_channels: Option<u8>,
        source_bits: Option<u8>,
        decoded_sample_rate: u32,
        decoded_channels: u8,
        decoded_bits: u8,
    },
    StreamStopped {
        source: &'a str,
        epoch: u64,
    },
    SourceTakeover {
        source: &'a str,
        media_kind: &'a str,
        epoch: u64,
        previous_source: Option<&'a str>,
        previous_media_kind: Option<&'a str>,
        previous_epoch: Option<u64>,
        reason: &'a str,
    },
    NowPlaying {
        source: &'a str,
        epoch: u64,
        title: Option<&'a str>,
        artist: Option<&'a str>,
        album: Option<&'a str>,
        genre: Option<&'a str>,
        duration_ms: Option<u32>,
    },
    CoverArt {
        source: &'a str,
        epoch: u64,
        path: &'a str,
    },
    Volume {
        source: &'a str,
        epoch: u64,
        db: f32,
        percent: u8,
    },
    Progress {
        source: &'a str,
        epoch: u64,
        position_ms: u64,
        duration_ms: u64,
    },
    PlaybackState {
        source: &'a str,
        epoch: u64,
        playing: bool,
    },
    VideoPlay {
        source: &'a str,
        epoch: u64,
        url: &'a str,
        start_position_ms: u64,
    },
    VideoSeek {
        source: &'a str,
        epoch: u64,
        position_ms: u64,
    },
    VideoRate {
        source: &'a str,
        epoch: u64,
        rate: f32,
    },
    VideoStop {
        source: &'a str,
        epoch: u64,
    },
    DlnaReady {
        port: u16,
        device_uuid: &'a str,
    },
    DlnaMedia {
        source: &'a str,
        epoch: u64,
        url: &'a str,
        title: Option<&'a str>,
        artist: Option<&'a str>,
        album: Option<&'a str>,
        artwork_url: Option<&'a str>,
        content_type: Option<&'a str>,
        bitrate_bps: Option<u64>,
        sample_rate: Option<u32>,
        bits_per_sample: Option<u16>,
        channels: Option<u16>,
        upnp_class: Option<&'a str>,
        media_kind: &'a str,
        duration_ms: Option<u64>,
        start_position_ms: u64,
        lyrics_text: Option<&'a str>,
        lyrics_uri: Option<&'a str>,
    },
    DlnaSeek {
        source: &'a str,
        epoch: u64,
        position_ms: u64,
    },
    DlnaRate {
        source: &'a str,
        epoch: u64,
        rate: f32,
    },
    DlnaStop {
        source: &'a str,
        epoch: u64,
    },
    DlnaVolume {
        source: &'a str,
        epoch: u64,
        percent: u8,
        muted: bool,
    },
    RemoteControlAvailable {
        source: &'a str,
        epoch: u64,
        commands: Vec<&'a str>,
        transport: &'a str,
        experimental: bool,
    },
    RemoteControlUnavailable {
        source: Option<&'a str>,
        epoch: Option<u64>,
        reason: &'a str,
    },
    CommandResult {
        request_id: Option<&'a str>,
        command: &'a str,
        ok: bool,
        message: Option<&'a str>,
    },
    Error {
        message: &'a str,
    },
    Log {
        level: &'a str,
        message: &'a str,
    },
}

pub type EventCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;

pub struct EventSink {
    writer: Mutex<Option<BufWriter<std::io::Stdout>>>,
    callback: Mutex<Option<EventCallback>>,
    #[cfg(test)]
    captured: Mutex<Vec<serde_json::Value>>,
}

impl EventSink {
    pub fn new() -> Self {
        Self {
            writer: Mutex::new(Some(BufWriter::new(std::io::stdout()))),
            callback: Mutex::new(None),
            #[cfg(test)]
            captured: Mutex::new(Vec::new()),
        }
    }

    pub fn with_callback(callback: EventCallback) -> Self {
        Self {
            writer: Mutex::new(None),
            callback: Mutex::new(Some(callback)),
            #[cfg(test)]
            captured: Mutex::new(Vec::new()),
        }
    }

    pub fn emit(&self, event: CoreEvent<'_>) {
        #[cfg(test)]
        if let Ok(value) = serde_json::to_value(&event)
            && let Ok(mut captured) = self.captured.lock()
        {
            captured.push(value);
        }

        let Ok(encoded) = serde_json::to_string(&event) else {
            return;
        };
        if let Ok(callback) = self.callback.lock()
            && let Some(callback) = callback.as_ref()
        {
            callback(encoded);
            return;
        }
        if let Ok(mut writer) = self.writer.lock()
            && let Some(writer) = writer.as_mut()
        {
            let _ = writeln!(writer, "{encoded}");
            let _ = writer.flush();
        }
    }

    #[cfg(test)]
    pub(crate) fn captured_events(&self) -> Vec<serde_json::Value> {
        self.captured
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::CoreEvent;
    use serde_json::json;

    #[test]
    fn aac_source_quality_does_not_invent_a_bit_depth() {
        let event = CoreEvent::StreamStarted {
            source: "airplay",
            epoch: 7,
            source_codec: Some("aac"),
            source_sample_rate: Some(44_100),
            source_channels: Some(2),
            source_bits: None,
            decoded_sample_rate: 48_000,
            decoded_channels: 2,
            decoded_bits: 32,
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "stream_started",
                "source": "airplay",
                "epoch": 7,
                "source_codec": "aac",
                "source_sample_rate": 44100,
                "source_channels": 2,
                "source_bits": null,
                "decoded_sample_rate": 48000,
                "decoded_channels": 2,
                "decoded_bits": 32
            })
        );
    }

    #[test]
    fn alac_source_quality_keeps_source_and_decoded_formats_separate() {
        let event = CoreEvent::StreamStarted {
            source: "airplay",
            epoch: 11,
            source_codec: Some("alac"),
            source_sample_rate: Some(44_100),
            source_channels: Some(2),
            source_bits: Some(16),
            decoded_sample_rate: 48_000,
            decoded_channels: 2,
            decoded_bits: 32,
        };
        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["source_bits"], 16);
        assert_eq!(value["source_sample_rate"], 44_100);
        assert_eq!(value["decoded_bits"], 32);
        assert_eq!(value["decoded_sample_rate"], 48_000);
    }

    #[test]
    fn dlna_media_event_keeps_renderer_metadata_in_one_contract() {
        let event = CoreEvent::DlnaMedia {
            source: "dlna",
            epoch: 19,
            url: "https://example.test/track.flac",
            title: Some("Track"),
            artist: Some("Artist"),
            album: Some("Album"),
            artwork_url: Some("https://example.test/cover.jpg"),
            content_type: Some("audio/flac"),
            bitrate_bps: Some(1_411_200),
            sample_rate: Some(44_100),
            bits_per_sample: Some(16),
            channels: Some(2),
            upnp_class: Some("object.item.audioItem.musicTrack"),
            media_kind: "audio",
            duration_ms: Some(185_250),
            start_position_ms: 2_000,
            lyrics_text: Some("[00:01.00]First line"),
            lyrics_uri: Some("https://example.test/track.lrc"),
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "dlna_media",
                "source": "dlna",
                "epoch": 19,
                "url": "https://example.test/track.flac",
                "title": "Track",
                "artist": "Artist",
                "album": "Album",
                "artwork_url": "https://example.test/cover.jpg",
                "content_type": "audio/flac",
                "bitrate_bps": 1411200,
                "sample_rate": 44100,
                "bits_per_sample": 16,
                "channels": 2,
                "upnp_class": "object.item.audioItem.musicTrack",
                "media_kind": "audio",
                "duration_ms": 185250,
                "start_position_ms": 2000,
                "lyrics_text": "[00:01.00]First line",
                "lyrics_uri": "https://example.test/track.lrc"
            })
        );
    }

    #[test]
    fn source_takeover_carries_both_sides_and_the_monotonic_epoch() {
        let event = CoreEvent::SourceTakeover {
            source: "dlna",
            media_kind: "video",
            epoch: 22,
            previous_source: Some("airplay"),
            previous_media_kind: Some("audio"),
            previous_epoch: Some(21),
            reason: "set_av_transport_uri",
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "source_takeover",
                "source": "dlna",
                "media_kind": "video",
                "epoch": 22,
                "previous_source": "airplay",
                "previous_media_kind": "audio",
                "previous_epoch": 21,
                "reason": "set_av_transport_uri"
            })
        );
    }

    #[test]
    fn mutable_media_events_are_scoped_to_source_epoch() {
        let event = CoreEvent::NowPlaying {
            source: "airplay",
            epoch: 31,
            title: Some("Track"),
            artist: Some("Artist"),
            album: None,
            genre: None,
            duration_ms: Some(180_000),
        };

        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "now_playing",
                "source": "airplay",
                "epoch": 31,
                "title": "Track",
                "artist": "Artist",
                "album": null,
                "genre": null,
                "duration_ms": 180000
            })
        );
    }
}
