mod audio;
mod dlna;
mod events;
mod host;
#[cfg(any(target_os = "android", test))]
mod network_identity;
mod pairing;
mod takeover;
mod video;

#[cfg(target_os = "android")]
mod android_jni;

pub use host::{Settings, desktop_main, parse_settings_from, run_receiver};
