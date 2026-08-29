use crate::events::{CoreEvent, EventSink};
use crate::takeover::{MediaLease, MediaSource, PlaybackArbiter};
use shairplay::{HlsHandler, HlsSession};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

const MAX_VIDEO_URL_LENGTH: usize = 8 * 1024;

#[derive(Debug)]
struct PlaybackSnapshot {
    position_seconds: f32,
    duration_seconds: f32,
    rate: f32,
    ready: bool,
    last_update: Instant,
    stopped: bool,
}

struct VideoSessionState {
    playback: Mutex<PlaybackSnapshot>,
    lease: Mutex<Option<MediaLease>>,
    url: String,
}

impl VideoSessionState {
    fn lease(&self) -> Option<MediaLease> {
        self.lease.lock().ok().and_then(|lease| *lease)
    }

    fn replace_lease(&self, lease: MediaLease) {
        if let Ok(mut current) = self.lease.lock() {
            *current = Some(lease);
        }
    }
}

impl PlaybackSnapshot {
    fn new(start_position: f32) -> Self {
        Self {
            position_seconds: start_position.max(0.0),
            duration_seconds: 0.0,
            rate: 1.0,
            ready: false,
            last_update: Instant::now(),
            stopped: false,
        }
    }

    fn current_position(&self) -> f32 {
        let elapsed = if self.rate > 0.0 && !self.stopped {
            self.last_update.elapsed().as_secs_f32() * self.rate
        } else {
            0.0
        };
        let position = (self.position_seconds + elapsed).max(0.0);
        if self.duration_seconds > 0.0 {
            position.min(self.duration_seconds)
        } else {
            position
        }
    }

    fn settle_clock(&mut self) {
        self.position_seconds = self.current_position();
        self.last_update = Instant::now();
    }
}

/// Bridges AirPlay HTTP video control requests to the WinUI media player.
///
/// Windows Media Foundation plays the URL in the UI process. The most recent
/// state reported by the UI is retained so `/playback-info` can accurately
/// answer the sending Apple device.
pub struct ReceiverVideoBridge {
    events: Arc<EventSink>,
    arbiter: Arc<PlaybackArbiter>,
    current: Arc<Mutex<Option<Weak<VideoSessionState>>>>,
}

impl ReceiverVideoBridge {
    pub fn new(events: Arc<EventSink>, arbiter: Arc<PlaybackArbiter>) -> Self {
        Self {
            events,
            arbiter,
            current: Arc::new(Mutex::new(None)),
        }
    }

    pub fn update_state(
        &self,
        position_ms: Option<u64>,
        duration_ms: Option<u64>,
        rate: Option<f32>,
        ready: Option<bool>,
    ) {
        let state = self
            .current
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(Weak::upgrade));
        let Some(state) = state else {
            return;
        };
        let Some(lease) = state.lease() else {
            return;
        };
        if !self.arbiter.is_current(lease) {
            return;
        }
        let Ok(mut snapshot) = state.playback.lock() else {
            return;
        };

        snapshot.settle_clock();
        if let Some(position_ms) = position_ms {
            snapshot.position_seconds = position_ms as f32 / 1000.0;
        }
        if let Some(duration_ms) = duration_ms {
            snapshot.duration_seconds = duration_ms as f32 / 1000.0;
        }
        if let Some(rate) = rate.filter(|value| value.is_finite()) {
            snapshot.rate = rate.max(0.0);
        }
        if let Some(ready) = ready {
            snapshot.ready = ready;
        }
        snapshot.last_update = Instant::now();
    }

    pub fn suspend_for_takeover(&self, lease: MediaLease) {
        let state = self
            .current
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(Weak::upgrade));
        let Some(state) = state else {
            return;
        };
        if state.lease() != Some(lease) {
            return;
        }
        if let Ok(mut snapshot) = state.playback.lock() {
            if snapshot.stopped {
                return;
            }
            snapshot.settle_clock();
            snapshot.rate = 0.0;
            snapshot.ready = false;
        }
        self.events.emit(CoreEvent::VideoRate {
            source: "airplay",
            epoch: lease.epoch(),
            rate: 0.0,
        });
    }
}

impl HlsHandler for ReceiverVideoBridge {
    fn on_play(&self, url: &str, start_position: f32) -> Box<dyn HlsSession> {
        let start_position = if start_position.is_finite() {
            start_position.max(0.0)
        } else {
            0.0
        };
        let valid_url = validate_video_url(url);
        let state = Arc::new(VideoSessionState {
            playback: Mutex::new(PlaybackSnapshot::new(start_position)),
            lease: Mutex::new(None),
            url: url.to_owned(),
        });
        if !valid_url {
            if let Ok(mut snapshot) = state.playback.lock() {
                snapshot.rate = 0.0;
                snapshot.stopped = true;
            }
            self.events.emit(CoreEvent::Error {
                message: "Rejected unsafe or invalid AirPlay video URL; only HTTP/HTTPS is allowed",
            });
            return Box::new(ReceiverVideoSession {
                state,
                events: Arc::clone(&self.events),
                arbiter: Arc::clone(&self.arbiter),
                current: Arc::clone(&self.current),
                valid_url: false,
            });
        }
        let (lease, _transition) = self.arbiter.begin_takeover(
            MediaSource::AirPlayVideo,
            "video",
            "airplay_video_stream",
            false,
        );
        state.replace_lease(lease);
        if let Ok(mut current) = self.current.lock() {
            *current = Some(Arc::downgrade(&state));
        }

        if valid_url {
            self.events.emit(CoreEvent::VideoPlay {
                source: "airplay",
                epoch: lease.epoch(),
                url,
                start_position_ms: seconds_to_millis(start_position),
            });
        } else {
            if let Ok(mut snapshot) = state.playback.lock() {
                snapshot.rate = 0.0;
                snapshot.stopped = true;
            }
            self.events.emit(CoreEvent::Error {
                message: "已拒绝不安全或无效的视频地址；仅允许 HTTP/HTTPS AirPlay 视频。",
            });
        }

        Box::new(ReceiverVideoSession {
            state,
            events: Arc::clone(&self.events),
            arbiter: Arc::clone(&self.arbiter),
            current: Arc::clone(&self.current),
            valid_url,
        })
    }
}

struct ReceiverVideoSession {
    state: Arc<VideoSessionState>,
    events: Arc<EventSink>,
    arbiter: Arc<PlaybackArbiter>,
    current: Arc<Mutex<Option<Weak<VideoSessionState>>>>,
    valid_url: bool,
}

impl ReceiverVideoSession {
    fn stop_once(&self) {
        // Serialize the current-session check, removal, and emitted event with
        // on_play's current-session replacement. This guarantees either
        // VideoStop(old) -> VideoPlay(new), or just VideoPlay(new); an old
        // session can never stop a newer video after it has become current.
        let Ok(mut current_slot) = self.current.lock() else {
            return;
        };
        let is_current = current_slot
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, &self.state));
        let should_emit = if let Ok(mut snapshot) = self.state.playback.lock() {
            if snapshot.stopped {
                false
            } else {
                snapshot.settle_clock();
                snapshot.rate = 0.0;
                snapshot.stopped = true;
                true
            }
        } else {
            false
        };
        if should_emit && self.valid_url && is_current {
            current_slot.take();
            let Some(lease) = self.state.lease() else {
                return;
            };
            if self.arbiter.release(lease) {
                self.events.emit(CoreEvent::VideoStop {
                    source: "airplay",
                    epoch: lease.epoch(),
                });
            }
        }
    }
}

impl HlsSession for ReceiverVideoSession {
    fn duration(&self) -> f32 {
        self.state
            .playback
            .lock()
            .map(|state| state.duration_seconds)
            .unwrap_or(0.0)
    }

    fn position(&self) -> f32 {
        self.state
            .playback
            .lock()
            .map(|state| state.current_position())
            .unwrap_or(0.0)
    }

    fn rate(&self) -> f32 {
        self.state
            .playback
            .lock()
            .map(|state| state.rate)
            .unwrap_or(0.0)
    }

    fn ready(&self) -> bool {
        self.state
            .playback
            .lock()
            .map(|state| state.ready)
            .unwrap_or(false)
    }

    fn seek(&mut self, position: f32) {
        let Some(lease) = self.state.lease() else {
            return;
        };
        if !position.is_finite() || !self.valid_url {
            return;
        }
        let position = position.max(0.0);
        let applied = self.arbiter.run_if_current(lease, || {
            if let Ok(mut snapshot) = self.state.playback.lock() {
                if snapshot.stopped {
                    return;
                }
                snapshot.position_seconds = if snapshot.duration_seconds > 0.0 {
                    position.min(snapshot.duration_seconds)
                } else {
                    position
                };
                snapshot.last_update = Instant::now();
            }
            self.events.emit(CoreEvent::VideoSeek {
                source: "airplay",
                epoch: lease.epoch(),
                position_ms: seconds_to_millis(position),
            });
        });
        if applied.is_none()
            && let Ok(mut snapshot) = self.state.playback.lock()
            && !snapshot.stopped
        {
            snapshot.position_seconds = if snapshot.duration_seconds > 0.0 {
                position.min(snapshot.duration_seconds)
            } else {
                position
            };
            snapshot.last_update = Instant::now();
        }
    }

    fn set_rate(&mut self, rate: f32) {
        let Some(lease) = self.state.lease() else {
            return;
        };
        if !rate.is_finite() || !self.valid_url {
            return;
        }
        let rate = rate.max(0.0);
        if rate > 0.0 && !self.arbiter.is_current(lease) {
            let (new_lease, transition) = self.arbiter.begin_takeover(
                MediaSource::AirPlayVideo,
                "video",
                "airplay_video_resume",
                false,
            );
            self.state.replace_lease(new_lease);
            let position_ms = if let Ok(mut snapshot) = self.state.playback.lock() {
                if snapshot.stopped {
                    drop(transition);
                    return;
                }
                snapshot.settle_clock();
                snapshot.rate = rate;
                snapshot.ready = false;
                seconds_to_millis(snapshot.position_seconds)
            } else {
                0
            };
            self.events.emit(CoreEvent::VideoPlay {
                source: "airplay",
                epoch: new_lease.epoch(),
                url: &self.state.url,
                start_position_ms: position_ms,
            });
            self.events.emit(CoreEvent::VideoRate {
                source: "airplay",
                epoch: new_lease.epoch(),
                rate,
            });
            drop(transition);
            return;
        }
        if !self.arbiter.is_current(lease) {
            if let Ok(mut snapshot) = self.state.playback.lock() {
                if snapshot.stopped {
                    return;
                }
                snapshot.settle_clock();
                snapshot.rate = rate;
            }
            return;
        }
        self.arbiter.run_if_current(lease, || {
            if let Ok(mut snapshot) = self.state.playback.lock() {
                if snapshot.stopped {
                    return;
                }
                snapshot.settle_clock();
                snapshot.rate = rate;
            }
            self.events.emit(CoreEvent::VideoRate {
                source: "airplay",
                epoch: lease.epoch(),
                rate,
            });
        });
    }

    fn stop(&mut self) {
        self.stop_once();
    }
}

impl Drop for ReceiverVideoSession {
    fn drop(&mut self) {
        self.stop_once();
    }
}

fn validate_video_url(url: &str) -> bool {
    if url.is_empty()
        || url.len() > MAX_VIDEO_URL_LENGTH
        || url.bytes().any(|byte| byte.is_ascii_control())
    {
        return false;
    }
    let lower = url
        .get(..8.min(url.len()))
        .unwrap_or(url)
        .to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn seconds_to_millis(seconds: f32) -> u64 {
    (seconds.max(0.0) as f64 * 1000.0)
        .round()
        .min(u64::MAX as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_urls_are_limited_to_http_and_https() {
        assert!(validate_video_url("https://example.test/video/master.m3u8"));
        assert!(validate_video_url("HTTP://192.0.2.1/video.mp4"));
        assert!(!validate_video_url("file:///C:/Windows/win.ini"));
        assert!(!validate_video_url(r"\\server\share\video.mp4"));
        assert!(!validate_video_url(
            "https://example.test/a.m3u8\r\nX-Test: injected"
        ));
    }

    #[test]
    fn running_snapshot_advances_but_paused_snapshot_does_not() {
        let mut state = PlaybackSnapshot::new(4.0);
        state.last_update = Instant::now() - std::time::Duration::from_secs(2);
        assert!(state.current_position() >= 5.9);
        state.settle_clock();
        state.rate = 0.0;
        let paused = state.current_position();
        state.last_update = Instant::now() - std::time::Duration::from_secs(2);
        assert!((state.current_position() - paused).abs() < 0.01);
    }

    #[test]
    fn stopping_an_old_session_does_not_clear_the_new_session() {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        let bridge = ReceiverVideoBridge::new(events, arbiter);
        let old = bridge.on_play("https://example.test/old.m3u8", 0.0);
        let mut current = bridge.on_play("https://example.test/new.m3u8", 0.0);

        drop(old);
        assert!(
            bridge
                .current
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some(),
            "dropping an old connection must preserve the newer video"
        );

        current.stop();
        assert!(bridge.current.lock().unwrap().is_none());
    }

    #[test]
    fn invalid_video_does_not_take_over_the_current_source() {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        arbiter.takeover(MediaSource::Dlna, "video", "test_dlna_play", false, |_| ());
        let bridge = ReceiverVideoBridge::new(events, Arc::clone(&arbiter));

        let invalid = bridge.on_play("file:///C:/Windows/win.ini", 0.0);

        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));
        assert!(bridge.current.lock().unwrap().is_none());
        drop(invalid);
    }

    #[test]
    fn preempted_video_caches_seek_and_only_explicit_play_reclaims() {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        let bridge = ReceiverVideoBridge::new(events, Arc::clone(&arbiter));
        let mut session = bridge.on_play("https://example.test/movie.m3u8", 0.0);
        let state = bridge
            .current
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .unwrap();

        arbiter.takeover(
            MediaSource::Dlna,
            "video",
            "test_dlna_takeover",
            false,
            |_| (),
        );
        session.seek(45.0);
        session.set_rate(0.0);

        {
            let snapshot = state.playback.lock().unwrap();
            assert!((snapshot.position_seconds - 45.0).abs() < 0.001);
            assert_eq!(snapshot.rate, 0.0);
        }
        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));

        session.set_rate(1.0);
        assert_eq!(arbiter.current_source(), Some(MediaSource::AirPlayVideo));
        assert!(arbiter.is_current(state.lease().unwrap()));
    }

    #[test]
    fn takeover_suspends_video_without_dropping_its_hls_session() {
        let events = Arc::new(EventSink::new());
        let arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&events)));
        let bridge = Arc::new(ReceiverVideoBridge::new(events, Arc::clone(&arbiter)));
        let weak_bridge = Arc::downgrade(&bridge);
        arbiter.register_suspender(MediaSource::AirPlayVideo, move |lease| {
            if let Some(bridge) = weak_bridge.upgrade() {
                bridge.suspend_for_takeover(lease);
            }
        });
        let mut session = bridge.on_play("https://example.test/movie.m3u8", 12.0);
        let state = bridge
            .current
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .unwrap();

        arbiter.takeover(
            MediaSource::Dlna,
            "video",
            "test_dlna_takeover",
            false,
            |_| (),
        );

        assert_eq!(arbiter.current_source(), Some(MediaSource::Dlna));
        assert_eq!(state.playback.lock().unwrap().rate, 0.0);
        assert!(
            bridge
                .current
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some()
        );

        session.set_rate(1.0);
        assert_eq!(arbiter.current_source(), Some(MediaSource::AirPlayVideo));
    }
}
