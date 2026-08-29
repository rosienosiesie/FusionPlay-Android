//! FusionPlay MiPlay receiver SDK.
//!
//! The SDK owns Lyra/mDNS discovery, MiPlay control, RTSP/RTP reception,
//! decryption, AAC decoding and native audio output. It does not start or call
//! MiPCAudio, MAFSvr, Xiaomi PC Manager, an account service, or any other
//! vendor process. The same Rust API is available on Windows, macOS and Android.

pub mod discovery;
pub mod lyra;
pub mod media;
pub mod protocol;

use anyhow::Result;
use discovery::{MiPlayAdvertisement, idm_short_id};
use lyra::{LyraControlServer, start_lyra_server};
pub use media::EventEmitter;
use protocol::{ControlHub, start_control_server};
pub use protocol::{DeviceIdentity, InterconnectStage, MediaAction, MediaControlOutcome};
use serde_json::json;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum MiPlayDeviceType {
    Vehicle = 5,
    #[default]
    Television = 2,
    Tablet = 18,
    Speaker = 4,
    DisplaySpeaker = 16,
}

impl MiPlayDeviceType {
    pub const fn protocol_value(self) -> u8 {
        self as u8
    }

    /// Device category used by the later Lyra physical-sync channel.
    ///
    /// Xiaomi uses a second enum here: TV=3, screen speaker=5 and vehicle=8.
    /// Tablet=2 is retained for completeness. Plain speakers have no Lyra
    /// category in the sender's table, so zero prevents the physical-sync
    /// record from replacing the authoritative Mi Connect type with TV.
    pub const fn lyra_protocol_value(self) -> u8 {
        match self {
            Self::Vehicle => 8,
            Self::Television => 3,
            Self::Tablet => 2,
            Self::Speaker => 0,
            Self::DisplaySpeaker => 5,
        }
    }

    pub const fn category_name(self) -> &'static str {
        match self {
            Self::Vehicle => "vehicle",
            Self::Television => "television",
            Self::Tablet => "tablet",
            Self::Speaker => "speaker",
            Self::DisplaySpeaker => "display_speaker",
        }
    }

    pub const fn model_name(self) -> &'static str {
        match self {
            Self::Vehicle => "Android Automotive",
            Self::Television => "Android TV",
            Self::Tablet => "Android Tablet",
            Self::Speaker => "Smart Speaker",
            Self::DisplaySpeaker => "Display Speaker",
        }
    }
}

impl TryFrom<i32> for MiPlayDeviceType {
    type Error = &'static str;

    fn try_from(value: i32) -> std::result::Result<Self, Self::Error> {
        match value {
            5 => Ok(Self::Vehicle),
            2 => Ok(Self::Television),
            18 => Ok(Self::Tablet),
            4 => Ok(Self::Speaker),
            16 => Ok(Self::DisplaySpeaker),
            _ => Err("unsupported MiPlay device type"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReceiverIdentity {
    /// Persistent UUID used to derive the anonymous IDM/didHash identity.
    pub idm_instance_id: String,
    /// Persistent eight-character uppercase hexadecimal Lyra instance id.
    pub lyra_instance_id: String,
    /// Persistent device id used only by the MiPlay control protocol.
    pub media_device_id: String,
}

impl ReceiverIdentity {
    pub fn new(
        idm_instance_id: impl Into<String>,
        lyra_instance_id: impl Into<String>,
        media_device_id: impl Into<String>,
    ) -> Self {
        Self {
            idm_instance_id: idm_instance_id.into(),
            lyra_instance_id: lyra_instance_id.into(),
            media_device_id: media_device_id.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    pub name: String,
    pub identity: ReceiverIdentity,
    pub local_ip: Ipv4Addr,
    pub interface_name: String,
    pub model: String,
    pub platform: String,
    /// Device category exposed consistently through Mi Connect discovery and
    /// the authenticated GetDeviceInfo control response.
    pub device_type: MiPlayDeviceType,
    /// Physical adapter address advertised to Xiaomi Bonjour clients.  This
    /// is separate from the persistent eight-character control identity.
    pub hardware_address: Option<String>,
    /// Native audio output selected by the host application. The value may be
    /// a CPAL device id or a human-readable endpoint name.
    pub output_device: Option<String>,
    /// Receiver-side volume restored by the host. MiPlay queries this value as
    /// soon as a route connects, so using a hard-coded 100 here would force the
    /// sender's remote-volume UI to maximum on every application restart.
    pub initial_volume_percent: u32,
    /// Enables local multicast delivery only for an explicit diagnostic
    /// probe. Production receivers keep this disabled to avoid self-traffic.
    pub diagnostic_multicast_loopback: bool,
}

impl ReceiverConfig {
    pub fn new(
        name: impl Into<String>,
        identity: ReceiverIdentity,
        local_ip: Ipv4Addr,
        interface_name: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            identity,
            local_ip,
            interface_name: interface_name.into(),
            model: default_model().to_owned(),
            platform: host_platform().to_owned(),
            device_type: MiPlayDeviceType::default(),
            hardware_address: None,
            output_device: None,
            initial_volume_percent: 50,
            diagnostic_multicast_loopback: false,
        }
    }

    pub fn with_output_device(mut self, output_device: Option<String>) -> Self {
        self.output_device = output_device.filter(|value| !value.trim().is_empty());
        self
    }

    pub fn with_device_type(mut self, device_type: MiPlayDeviceType) -> Self {
        self.device_type = device_type;
        self.model = device_type.model_name().to_owned();
        self
    }

    pub fn with_initial_volume_percent(mut self, volume_percent: u32) -> Self {
        self.initial_volume_percent = volume_percent.min(100);
        self
    }

    pub fn with_hardware_address(mut self, hardware_address: Option<String>) -> Self {
        self.hardware_address = hardware_address.filter(|value| !value.trim().is_empty());
        self
    }

    pub fn with_diagnostic_multicast_loopback(mut self, enabled: bool) -> Self {
        self.diagnostic_multicast_loopback = enabled;
        self
    }
}

#[derive(Clone)]
pub struct ReceiverController {
    hub: ControlHub,
    shutdown: Arc<AtomicBool>,
}

impl ReceiverController {
    pub fn send(&self, action: MediaAction) -> Result<()> {
        self.hub.send(action)
    }

    pub fn set_volume(&self, percent: u8) -> Result<()> {
        self.hub.set_volume(percent)
    }

    pub fn send_confirmed(
        &self,
        action: MediaAction,
        timeout: std::time::Duration,
    ) -> Result<MediaControlOutcome> {
        self.hub.send_confirmed(action, timeout)
    }

    /// Immediately mutes receiver-side MiPlay output without tearing down the
    /// phone connection. The suspension is inherited by replacement media
    /// sessions until [`Self::resume_output`] explicitly releases ownership.
    pub fn suspend_output(&self) {
        self.hub.suspend_output();
    }

    pub fn resume_output(&self) {
        self.hub.resume_output();
    }

    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.hub.shutdown_sessions();
    }

    pub fn is_stopped(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

pub struct MiPlayReceiver {
    advertisement: MiPlayAdvertisement,
    lyra_control: LyraControlServer,
    controller: ReceiverController,
}

impl MiPlayReceiver {
    pub fn start(config: ReceiverConfig, events: EventEmitter) -> Result<Self> {
        let volume_percent = Arc::new(AtomicU32::new(config.initial_volume_percent.min(100)));
        let shutdown = Arc::new(AtomicBool::new(false));
        let identity = DeviceIdentity {
            // MiPCAudio returns the same three-character value from
            // GetDeviceInfo that `_mi-connect` publishes as idHash. Lyra has
            // a separate eight-character instance id; using it here prevents
            // HyperOS from correlating the two discovery paths and can expose
            // duplicate routes for one receiver.
            device_id: idm_short_id(&config.identity.idm_instance_id),
            device_name: config.name.clone(),
            model: config.model.clone(),
            platform: config.platform.clone(),
            device_type: config.device_type,
        };
        let hub = start_control_server(
            config.local_ip,
            identity,
            config.output_device.clone(),
            Arc::clone(&volume_percent),
            Arc::clone(&shutdown),
            Arc::clone(&events),
        )?;
        let lyra_control = match start_lyra_server(
            config.local_ip,
            config.identity.lyra_instance_id.clone(),
            config.name.clone(),
            config.platform.clone(),
            config.device_type,
            Arc::clone(&shutdown),
            Arc::clone(&events),
        ) {
            Ok(server) => server,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                return Err(error);
            }
        };
        // `_miplay_lan` is intentionally not a runtime option because HyperOS
        // exposes it as a second generic television. Account-free MiPlay uses
        // query-scoped Lyra and Mi Connect routes instead, so both identities
        // first appear while MiPlay's listener is active and no permanent
        // legacy TV identity is published.
        events(json!({
            "event": "miplay_lan_removed",
            "protocol": "xiaomi_miplay",
            "reason": "query_scoped_lyra_and_mi_connect_without_legacy_lan_tv",
        }));
        let advertisement = match MiPlayAdvertisement::register(
            &config.name,
            &config.identity.lyra_instance_id,
            &config.identity.idm_instance_id,
            config
                .hardware_address
                .as_deref()
                .unwrap_or(&config.identity.media_device_id),
            config.device_type,
            config.local_ip,
            &config.interface_name,
            config.diagnostic_multicast_loopback,
            Arc::clone(&events),
        ) {
            Ok(advertisement) => advertisement,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                return Err(error);
            }
        };
        let controller = ReceiverController { hub, shutdown };
        events(json!({
            "event": "receiver_ready",
            "protocol": "xiaomi_miplay_v3",
            "name": config.name,
            "address": config.local_ip.to_string(),
            "port": lyra_control.port(),
            "interface": config.interface_name,
            "platform": config.platform,
            "device_type": config.device_type.protocol_value(),
            "output_device": config.output_device,
            "mdns_service": advertisement.fullname(),
            "legacy_lan_discovery": false,
            "network_scope": "local_subnet",
            "official_wired_support": "undocumented",
            "external_service_required": false,
            "identity_provenance": "local_system",
            "lyra_instance_id": config.identity.lyra_instance_id,
            "media_device_id": config.identity.media_device_id,
            "vendor_attestation_verified": false,
            "playable": false,
            "implementation": "fusionplay_cross_platform_rust_sdk",
        }));
        Ok(Self {
            advertisement,
            lyra_control,
            controller,
        })
    }

    pub fn controller(&self) -> ReceiverController {
        self.controller.clone()
    }

    pub fn service_name(&self) -> &str {
        self.advertisement.fullname()
    }

    pub const fn lyra_command_port(&self) -> u16 {
        self.lyra_control.port()
    }
}

impl Drop for MiPlayReceiver {
    fn drop(&mut self) {
        self.controller.stop();
    }
}

pub const fn host_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "android") {
        "Android"
    } else {
        "Rust"
    }
}

pub const fn default_model() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows PC"
    } else if cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(target_os = "android") {
        "Android TV"
    } else {
        "FusionPlay receiver"
    }
}
