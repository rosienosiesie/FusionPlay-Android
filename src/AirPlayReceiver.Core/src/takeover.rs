use crate::events::{CoreEvent, EventSink};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Freezes an active source without tearing down its protocol connection.
///
/// A suspended source keeps its protocol-owned session state and may later
/// claim a fresh playback lease when the sender explicitly resumes.
type SuspendHook = Arc<dyn Fn(MediaLease) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaSource {
    AirPlayAudio = 1,
    AirPlayVideo = 2,
    Dlna = 3,
    XiaomiMiPlay = 4,
}

impl MediaSource {
    pub fn protocol_name(self) -> &'static str {
        match self {
            Self::AirPlayAudio | Self::AirPlayVideo => "airplay",
            Self::Dlna => "dlna",
            Self::XiaomiMiPlay => "xiaomi_miplay",
        }
    }

    fn from_discriminant(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::AirPlayAudio),
            2 => Some(Self::AirPlayVideo),
            3 => Some(Self::Dlna),
            4 => Some(Self::XiaomiMiPlay),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaLease {
    source: MediaSource,
    epoch: u64,
}

impl MediaLease {
    pub fn epoch(self) -> u64 {
        self.epoch
    }
}

#[derive(Debug, Clone, Copy)]
struct Ownership {
    lease: MediaLease,
    media_kind: &'static str,
}

#[derive(Default)]
struct SuspendHooks {
    airplay_audio: Option<SuspendHook>,
    airplay_video: Option<SuspendHook>,
    dlna: Option<SuspendHook>,
    xiaomi_miplay: Option<SuspendHook>,
}

impl SuspendHooks {
    fn slot_mut(&mut self, source: MediaSource) -> &mut Option<SuspendHook> {
        match source {
            MediaSource::AirPlayAudio => &mut self.airplay_audio,
            MediaSource::AirPlayVideo => &mut self.airplay_video,
            MediaSource::Dlna => &mut self.dlna,
            MediaSource::XiaomiMiPlay => &mut self.xiaomi_miplay,
        }
    }

    fn get(&self, source: MediaSource) -> Option<SuspendHook> {
        match source {
            MediaSource::AirPlayAudio => self.airplay_audio.clone(),
            MediaSource::AirPlayVideo => self.airplay_video.clone(),
            MediaSource::Dlna => self.dlna.clone(),
            MediaSource::XiaomiMiPlay => self.xiaomi_miplay.clone(),
        }
    }
}

#[derive(Default)]
struct ArbiterState {
    current: Option<Ownership>,
    hooks: SuspendHooks,
}

/// Serializes media-source changes and rejects cleanup from stale sessions.
pub struct PlaybackArbiter {
    events: Arc<EventSink>,
    transition: Mutex<()>,
    state: Mutex<ArbiterState>,
    next_epoch: AtomicU64,
    current_epoch: AtomicU64,
    current_source: AtomicU8,
}

pub struct TakeoverGuard<'a> {
    _transition: MutexGuard<'a, ()>,
}

impl PlaybackArbiter {
    pub fn new(events: Arc<EventSink>) -> Self {
        Self {
            events,
            transition: Mutex::new(()),
            state: Mutex::new(ArbiterState::default()),
            next_epoch: AtomicU64::new(1),
            current_epoch: AtomicU64::new(0),
            current_source: AtomicU8::new(0),
        }
    }

    pub fn register_suspender(
        &self,
        source: MediaSource,
        suspend: impl Fn(MediaLease) + Send + Sync + 'static,
    ) {
        if let Ok(mut state) = self.state.lock() {
            *state.hooks.slot_mut(source) = Some(Arc::new(suspend));
        }
    }

    /// Runs an entire source transition as one critical section:
    /// old-source suspension, takeover event, then new-source activation.
    pub fn takeover<R>(
        &self,
        source: MediaSource,
        media_kind: &'static str,
        reason: &'static str,
        suspend_previous_same_source: bool,
        activate: impl FnOnce(MediaLease) -> R,
    ) -> R {
        let (lease, _transition) =
            self.begin_takeover(source, media_kind, reason, suspend_previous_same_source);
        activate(lease)
    }

    pub fn begin_takeover(
        &self,
        source: MediaSource,
        media_kind: &'static str,
        reason: &'static str,
        suspend_previous_same_source: bool,
    ) -> (MediaLease, TakeoverGuard<'_>) {
        let transition = self
            .transition
            .lock()
            .expect("playback takeover mutex poisoned");
        self.begin_takeover_locked(
            transition,
            source,
            media_kind,
            reason,
            suspend_previous_same_source,
        )
    }

    /// Begins a takeover only when `allow` still succeeds while the global
    /// transition lock is held. Protocol adapters use this to reject a stale
    /// request without a check-then-takeover race.
    pub fn begin_takeover_checked(
        &self,
        source: MediaSource,
        media_kind: &'static str,
        reason: &'static str,
        suspend_previous_same_source: bool,
        allow: impl FnOnce() -> bool,
    ) -> Option<(MediaLease, TakeoverGuard<'_>)> {
        let transition = self
            .transition
            .lock()
            .expect("playback takeover mutex poisoned");
        if !allow() {
            return None;
        }
        Some(self.begin_takeover_locked(
            transition,
            source,
            media_kind,
            reason,
            suspend_previous_same_source,
        ))
    }

    fn begin_takeover_locked<'a>(
        &'a self,
        transition: MutexGuard<'a, ()>,
        source: MediaSource,
        media_kind: &'static str,
        reason: &'static str,
        suspend_previous_same_source: bool,
    ) -> (MediaLease, TakeoverGuard<'a>) {
        let epoch = self.next_epoch.fetch_add(1, Ordering::Relaxed);
        let lease = MediaLease { source, epoch };
        let (previous, suspend_hook) = {
            let mut state = self.state.lock().expect("playback arbiter mutex poisoned");
            let previous = state.current.replace(Ownership { lease, media_kind });
            let should_suspend = previous
                .is_some_and(|old| old.lease.source != source || suspend_previous_same_source);
            let suspend_hook = previous
                .filter(|_| should_suspend)
                .and_then(|old| state.hooks.get(old.lease.source));
            self.current_source.store(source as u8, Ordering::Release);
            self.current_epoch.store(epoch, Ordering::Release);
            (previous, suspend_hook)
        };

        if let (Some(previous), Some(suspend_hook)) = (previous, suspend_hook) {
            suspend_hook(previous.lease);
        }

        self.events.emit(CoreEvent::SourceTakeover {
            source: source.protocol_name(),
            media_kind,
            epoch,
            previous_source: previous.map(|old| old.lease.source.protocol_name()),
            previous_media_kind: previous.map(|old| old.media_kind),
            previous_epoch: previous.map(|old| old.lease.epoch),
            reason,
        });
        (
            lease,
            TakeoverGuard {
                _transition: transition,
            },
        )
    }

    pub fn is_current(&self, lease: MediaLease) -> bool {
        self.current_epoch.load(Ordering::Acquire) == lease.epoch
            && self.current_source.load(Ordering::Acquire) == lease.source as u8
    }

    pub fn run_if_current<R>(&self, lease: MediaLease, action: impl FnOnce() -> R) -> Option<R> {
        let _transition = self.transition.lock().ok()?;
        self.is_current(lease).then(action)
    }

    pub fn finish_if_current<R>(&self, lease: MediaLease, finish: impl FnOnce() -> R) -> Option<R> {
        let _transition = self.transition.lock().ok()?;
        if !self.is_current(lease) {
            return None;
        }
        let result = finish();
        self.release(lease);
        Some(result)
    }

    pub fn current_source(&self) -> Option<MediaSource> {
        MediaSource::from_discriminant(self.current_source.load(Ordering::Acquire))
    }

    pub fn current_lease(&self, source: MediaSource) -> Option<MediaLease> {
        let state = self.state.lock().ok()?;
        state
            .current
            .filter(|current| current.lease.source == source)
            .map(|current| current.lease)
    }

    /// Clears ownership only when the full source+epoch lease still matches.
    pub fn release(&self, lease: MediaLease) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.current.map(|current| current.lease) != Some(lease) {
            return false;
        }
        state.current = None;
        self.current_epoch.store(0, Ordering::Release);
        self.current_source.store(0, Ordering::Release);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn cross_source_takeover_suspends_previous_owner() {
        let arbiter = PlaybackArbiter::new(Arc::new(EventSink::new()));
        let stopped = Arc::new(AtomicUsize::new(0));
        let stopped_for_hook = Arc::clone(&stopped);
        arbiter.register_suspender(MediaSource::AirPlayAudio, move |_| {
            stopped_for_hook.fetch_add(1, Ordering::Relaxed);
        });

        let old = arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "airplay_audio_stream",
            false,
            |lease| lease,
        );
        let new = arbiter.takeover(
            MediaSource::Dlna,
            "video",
            "set_av_transport_uri",
            false,
            |lease| lease,
        );

        assert_eq!(stopped.load(Ordering::Relaxed), 1);
        assert!(!arbiter.is_current(old));
        assert!(arbiter.is_current(new));
    }

    #[test]
    fn stale_release_cannot_clear_a_newer_same_source_lease() {
        let arbiter = PlaybackArbiter::new(Arc::new(EventSink::new()));
        let old = arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "airplay_audio_stream",
            false,
            |lease| lease,
        );
        let new = arbiter.takeover(
            MediaSource::AirPlayAudio,
            "audio",
            "airplay_audio_stream",
            false,
            |lease| lease,
        );

        assert!(!arbiter.release(old));
        assert!(arbiter.is_current(new));
        assert!(arbiter.release(new));
        assert_eq!(arbiter.current_source(), None);
    }

    #[test]
    fn same_source_suspend_is_explicitly_opt_in() {
        let arbiter = PlaybackArbiter::new(Arc::new(EventSink::new()));
        let stopped = Arc::new(AtomicUsize::new(0));
        let stopped_for_hook = Arc::clone(&stopped);
        arbiter.register_suspender(MediaSource::Dlna, move |_| {
            stopped_for_hook.fetch_add(1, Ordering::Relaxed);
        });
        arbiter.takeover(
            MediaSource::Dlna,
            "audio",
            "set_av_transport_uri",
            true,
            |_| {},
        );
        arbiter.takeover(
            MediaSource::Dlna,
            "video",
            "set_av_transport_uri",
            true,
            |_| {},
        );

        assert_eq!(stopped.load(Ordering::Relaxed), 1);
    }
}
