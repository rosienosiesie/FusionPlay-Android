use crate::MiPlayDeviceType;
use crate::media::EventEmitter;
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MDNS_PORT: u16 = 5353;
const LYRA_CONTROL_PORT: u16 = 5353;
const MI_CONNECT_DISCOVERY_PORT: u16 = 56_666;
const LYRA_TTL_SECONDS: u32 = 120;
const ON_DEMAND_MI_CONNECT_TTL_SECONDS: u32 = 10;
const ON_DEMAND_MI_CONNECT_IDLE_TIMEOUT: Duration = Duration::from_secs(12);
const DISCOVERY_ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(60);
const LYRA_SERVICE_TYPE: &str = "_lyra-mdns._udp.local.";
const LYRA_SERVICE_QNAME: &[u8] = b"\x0a_lyra-mdns\x04_udp\x05local\x00";
const MI_CONNECT_SERVICE_TYPE: &str = "_mi-connect._udp.local.";
const MI_CONNECT_SERVICE_QNAME: &[u8] = b"\x0b_mi-connect\x04_udp\x05local\x00";
const LYRA_SERVICE_NAME: &str = "_lyra-mdns._udp.local";
const MI_CONNECT_SERVICE_NAME: &str = "_mi-connect._udp.local";
// Both records must first appear in the same active MiPlay scan. Publishing
// only `_mi-connect` is not sufficient on current HyperOS: the sender may keep
// the unmatched route as a generic TV or reject it after capability checking.
// Keep the two service identities shaped like the last known single-route
// Android build (1.1.7): Lyra provides the discovery handshake while the
// canonical picker entry is owned by the legacy-named Mi Connect endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DiscoveryPlatformPolicy {
    publish_app_lyra: bool,
}

const fn discovery_platform_policy(_is_android: bool) -> DiscoveryPlatformPolicy {
    DiscoveryPlatformPolicy {
        publish_app_lyra: true,
    }
}

const fn persistent_mi_connect_discovery(_is_android: bool, override_enabled: bool) -> bool {
    // Match the documented desktop production profile on Android as well.
    // Persistent `dev=2/18` publication is what makes Fusion Device Center
    // retain an app receiver as a real television or tablet. Production keeps
    // both records query-scoped unless the diagnostic override is explicit.
    override_enabled
}

const DISCOVERY_PLATFORM_POLICY: DiscoveryPlatformPolicy =
    discovery_platform_policy(cfg!(target_os = "android"));
const APP_LYRA_DISCOVERY_ENABLED: bool = DISCOVERY_PLATFORM_POLICY.publish_app_lyra;
#[cfg(not(windows))]
const LYRA_PEER_BROWSER_ENABLED: bool = APP_LYRA_DISCOVERY_ENABLED;
fn lyra_discovery_device_type(device_type: MiPlayDeviceType) -> u8 {
    match device_type {
        // NetBus/Lyra and Mi Connect use different public enums. HyperOS'
        // MiPlay adapter maps TV 3 -> 2, automotive 8 -> 5 and sound 5 -> 16;
        // Pad remains the NetBus PAD value 2. Advertising the old fixed value
        // 0x15 made every Lyra record a generic PC while Mi Connect retained
        // the user's selected category, so one physical receiver could be
        // reported as two logical devices. Keep the discovery projection tied
        // to the selected identity instead.
        MiPlayDeviceType::Vehicle => 8,
        MiPlayDeviceType::Television => 3,
        MiPlayDeviceType::Tablet => 2,
        // NetBus has one public sound category. Mi Connect and command 31 keep
        // distinguishing plain speakers (4) from display speakers (16).
        MiPlayDeviceType::Speaker | MiPlayDeviceType::DisplaySpeaker => 5,
    }
}
// Builds used during the protocol bring-up copied this Lyra instance id from
// an official MiPCAudio capture.  HyperOS can retain that PTR record across an
// application upgrade and then display the captured computer name as a second
// route next to FusionPlay's current `_mi-connect` identity.  Emit a targeted
// goodbye for the retired instance while keeping all production identities
// derived from this computer.
const RETIRED_LYRA_INSTANCE_NAMES: &[&str] = &["2433CD31", "D4349E25"];
// Retire development-only Mi Connect owners while preserving the dynamically
// derived 1.1.7-compatible owner used by the current installation.
#[cfg(not(windows))]
const RETIRED_MI_CONNECT_INSTANCE_NAMES: &[&str] = &["Xiaomi17ProMax(_TnyWeXvGA)"];
#[derive(Clone, Debug, Eq, PartialEq)]
struct IdmIdentity {
    did_hash: String,
    short_id: String,
    instance_suffix: String,
}

/// Derive the anonymous (not signed in to a Xiaomi account) IDM identity.
///
/// Xiaomi's Windows IDM runtime hashes its persistent instance id with
/// SHA-256, encodes the complete digest as unpadded Base64URL, then uses the
/// first three characters as idHash/service-id prefix and the first ten as
/// the Bonjour instance suffix.  Keeping all three fields derived from one
/// stable seed is important: recent senders reject an otherwise valid Lyra
/// record when the `_mi-connect` identity fields disagree with each other.
fn idm_identity(stable_seed: &str) -> IdmIdentity {
    let digest = Sha256::digest(stable_seed.as_bytes());
    let did_hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    IdmIdentity {
        short_id: did_hash[..3].to_owned(),
        instance_suffix: did_hash[..10].to_owned(),
        did_hash,
    }
}

pub(crate) fn idm_short_id(stable_seed: &str) -> String {
    idm_identity(stable_seed).short_id
}

fn sanitize_protocol_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push_str("FusionPlay");
    }
    if sanitized.len() > 45 {
        sanitized.truncate(45);
    }
    sanitized
}
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x00fb);

struct DiscoveryResponses {
    lyra: Vec<u8>,
    mi_connect: Vec<u8>,
    lyra_instance_name: String,
    mi_connect_instance_name: String,
    retired_mi_connect_goodbyes: Vec<Vec<u8>>,
}

pub struct MiPlayAdvertisement {
    fullname: String,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MiPlayAdvertisement {
    pub fn register(
        receiver_name: &str,
        lyra_instance_id: &str,
        idm_instance_id: &str,
        media_device_id: &str,
        device_type: MiPlayDeviceType,
        local_ip: Ipv4Addr,
        interface_name: &str,
        diagnostic_multicast_loopback: bool,
        events: EventEmitter,
    ) -> Result<Self> {
        let protocol_name = sanitize_protocol_name(receiver_name);
        let instance = lyra_instance(lyra_instance_id, local_ip);
        let link_local_v6 = find_link_local_ipv6(interface_name, local_ip);
        let mi_connect_ipv6 = find_mi_connect_ipv6(interface_name, local_ip, link_local_v6);
        let interface_inventory = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .map(|interface| {
                json!({
                    "name": interface.name,
                    "index": interface.index,
                    "address": interface.ip().to_string(),
                    "link_local_v6": matches!(interface.ip(), IpAddr::V6(address) if address.is_unicast_link_local()),
                })
            })
            .collect::<Vec<_>>();
        events(json!({
            "event": "discovery_interface_inventory",
            "protocol": "xiaomi_miplay",
            "requested_address": local_ip.to_string(),
            "requested_interface": interface_name,
            "selected_link_local_v6": link_local_v6.map(|address| address.to_string()),
            "selected_mi_connect_ipv6": mi_connect_ipv6.map(|address| address.to_string()),
            "interfaces": interface_inventory,
        }));
        let idm_identity = idm_identity(idm_instance_id);
        events(json!({
            "event": "discovery_identity_ready",
            "protocol": "xiaomi_miplay",
            "identity_mode": "local_unlogged",
            "did_hash": idm_identity.did_hash,
            "id_hash": idm_identity.short_id,
            "instance_suffix": idm_identity.instance_suffix,
            "service_id": format!("{}5", idm_identity.short_id),
            "mi_connect_device_type": device_type.protocol_value(),
            "lyra_discovery_device_type": lyra_discovery_device_type(device_type),
            "account_required": false,
            "vendor_service_required": false,
        }));
        let mi_connect_instance = mi_connect_instance_for_platform(
            cfg!(target_os = "android"),
            &protocol_name,
            idm_instance_id,
            &instance,
        );
        let shutdown = Arc::new(AtomicBool::new(false));

        #[cfg(windows)]
        let worker = spawn_windows_dnssd_responder(
            // DNS instance labels are sanitized separately below, but Lyra's
            // AppData and TXT name are UTF-8 fields. Passing the sanitized
            // DNS label here replaced an all-CJK Windows device name with
            // "FusionPlay" and made the wire response differ from MAFSvr.
            receiver_name.to_owned(),
            instance.clone(),
            mi_connect_instance,
            idm_instance_id.to_owned(),
            media_device_id.to_owned(),
            device_type,
            local_ip,
            link_local_v6,
            mi_connect_ipv6,
            interface_name.to_owned(),
            diagnostic_multicast_loopback,
            Arc::clone(&shutdown),
            events,
        )?;

        #[cfg(not(windows))]
        let worker = {
            let mut retired_mi_connect_instances = RETIRED_MI_CONNECT_INSTANCE_NAMES
                .iter()
                .map(|instance| (*instance).to_owned())
                .collect::<Vec<_>>();
            if cfg!(target_os = "android") {
                // A short-lived development build reused the Lyra instance for
                // Mi Connect. Explicitly evict that owner when migrating back
                // to the proven legacy-named single-route profile.
                retired_mi_connect_instances.push(instance.clone());
            }
            retired_mi_connect_instances.sort();
            retired_mi_connect_instances.dedup();
            retired_mi_connect_instances.retain(|retired| retired != &mi_connect_instance);
            let retired_mi_connect_goodbyes = retired_mi_connect_instances
                .into_iter()
                .filter_map(|retired_instance| {
                    zero_dns_record_ttls(&build_mi_connect_response(
                        receiver_name,
                        &retired_instance,
                        idm_instance_id,
                        media_device_id,
                        device_type,
                        local_ip,
                        mi_connect_ipv6,
                    ))
                })
                .collect();
            let responses = Arc::new(DiscoveryResponses {
                lyra: build_lyra_response(
                    receiver_name,
                    &instance,
                    media_device_id,
                    device_type,
                    local_ip,
                    link_local_v6,
                    interface_name,
                ),
                mi_connect: build_mi_connect_response(
                    receiver_name,
                    &mi_connect_instance,
                    idm_instance_id,
                    media_device_id,
                    device_type,
                    local_ip,
                    mi_connect_ipv6,
                ),
                lyra_instance_name: instance.clone(),
                mi_connect_instance_name: mi_connect_instance.clone(),
                retired_mi_connect_goodbyes,
            });
            let socket = create_responder_socket(local_ip, diagnostic_multicast_loopback)?;
            let worker_shutdown = Arc::clone(&shutdown);
            let worker_responses = Arc::clone(&responses);
            let browser_link_local_v6 = link_local_v6;
            let browser_interface_name = interface_name.to_owned();
            thread::Builder::new()
                .name("miplay-lyra-discovery".to_owned())
                .spawn(move || {
                    let browser_ipv4 = LYRA_PEER_BROWSER_ENABLED
                        .then(|| {
                            create_ipv4_browser_socket(local_ip).and_then(|browser_socket| {
                                spawn_lyra_peer_browser(
                                    browser_socket,
                                    "ipv4",
                                    SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT)),
                                    Arc::clone(&worker_shutdown),
                                    Arc::clone(&events),
                                )
                            })
                        })
                        .and_then(Result::ok);
                    let browser_ipv6 = LYRA_PEER_BROWSER_ENABLED
                        .then_some(browser_link_local_v6)
                        .flatten()
                        .and_then(|link_local_ip| {
                            let interface_index = if_addrs::get_if_addrs()
                                .ok()
                                .and_then(|interfaces| {
                                    interfaces
                                        .into_iter()
                                        .find(|interface| {
                                            interface.name == browser_interface_name
                                                && interface.ip() == IpAddr::V4(local_ip)
                                        })
                                        .and_then(|interface| interface.index)
                                })
                                .unwrap_or(0);
                            create_ipv6_browser_socket(interface_index, link_local_ip)
                                .and_then(|browser_socket| {
                                    spawn_lyra_peer_browser(
                                        browser_socket,
                                        "ipv6",
                                        SocketAddr::V6(SocketAddrV6::new(
                                            MDNS_GROUP_V6,
                                            MDNS_PORT,
                                            0,
                                            interface_index,
                                        )),
                                        Arc::clone(&worker_shutdown),
                                        Arc::clone(&events),
                                    )
                                })
                                .ok()
                        });
                    responder_loop(
                        socket,
                        local_ip,
                        worker_responses,
                        Arc::clone(&worker_shutdown),
                        Arc::clone(&events),
                    );
                    if let Some(worker) = browser_ipv4 {
                        let _ = worker.join();
                    }
                    if let Some(worker) = browser_ipv6 {
                        let _ = worker.join();
                    }
                })
                .context("spawn Lyra discovery responder")?
        };
        Ok(Self {
            fullname: format!("{instance}.{LYRA_SERVICE_TYPE}"),
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for MiPlayAdvertisement {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(windows)]
const DNS_QUERY_REQUEST_VERSION1: u32 = 1;
#[cfg(windows)]
const DNS_REQUEST_PENDING: u32 = 9506;
// Windows DNS-SD emits a second, incomplete response for the same instance:
// PTR/SRV/TXT without the A/AAAA records present in Xiaomi's MiPCAudio wire
// response. Some HyperOS senders cache that response last and never advance
// to the embedded UDP Lyra command endpoint. The shared exact responder has been verified to receive
// external multicast queries while the Windows DNS Client owns port 5353, so
// keep the system registration disabled and expose one authoritative answer.
#[cfg(windows)]
const ENABLE_WINDOWS_DNS_SD_REGISTRATION: bool = false;

#[cfg(windows)]
#[repr(C)]
struct DnsServiceRegisterRequest {
    version: u32,
    interface_index: u32,
    service_instance: *mut std::ffi::c_void,
    completion_callback:
        Option<unsafe extern "system" fn(u32, *mut std::ffi::c_void, *mut std::ffi::c_void)>,
    query_context: *mut std::ffi::c_void,
    credentials: *mut std::ffi::c_void,
    unicast_enabled: i32,
}

#[cfg(windows)]
struct DnsServiceCallbackContext {
    sender: Mutex<Option<mpsc::SyncSender<u32>>>,
}

#[cfg(windows)]
unsafe extern "system" fn dns_service_complete(
    status: u32,
    query_context: *mut std::ffi::c_void,
    instance: *mut std::ffi::c_void,
) {
    // Windows returns a separately allocated copy to every completion
    // callback. It is not the registration instance retained in Self.
    if !instance.is_null() {
        // SAFETY: DNS_SERVICE_REGISTER_COMPLETE transfers ownership of this
        // callback copy to the application.
        unsafe { DnsServiceFreeInstance(instance) };
    }
    if query_context.is_null() {
        return;
    }
    // SAFETY: query_context points to the boxed callback context retained by
    // WindowsDnsSdService until registration/deregistration has completed.
    let context = unsafe { &*(query_context.cast::<DnsServiceCallbackContext>()) };
    if let Ok(mut sender) = context.sender.lock()
        && let Some(sender) = sender.take()
    {
        let _ = sender.send(status);
    }
}

#[cfg(windows)]
#[link(name = "dnsapi")]
unsafe extern "system" {
    fn DnsServiceConstructInstance(
        service_name: *const u16,
        host_name: *const u16,
        ip4_address: *const u32,
        ip6_address: *const std::ffi::c_void,
        port: u16,
        priority: u16,
        weight: u16,
        property_count: u32,
        keys: *const *const u16,
        values: *const *const u16,
    ) -> *mut std::ffi::c_void;
    fn DnsServiceFreeInstance(instance: *mut std::ffi::c_void);
    fn DnsServiceRegister(
        request: *mut DnsServiceRegisterRequest,
        cancel: *mut std::ffi::c_void,
    ) -> u32;
    fn DnsServiceDeRegister(
        request: *mut DnsServiceRegisterRequest,
        cancel: *mut std::ffi::c_void,
    ) -> u32;
}

#[cfg(windows)]
struct WindowsDnsSdService {
    instance: *mut std::ffi::c_void,
    request: Option<Box<DnsServiceRegisterRequest>>,
    callback_context: Option<Box<DnsServiceCallbackContext>>,
}

#[cfg(windows)]
impl WindowsDnsSdService {
    fn register(
        service_name: &str,
        host_name: &str,
        port: u16,
        interface_index: u32,
        local_ip: Ipv4Addr,
        properties: &[(String, String)],
    ) -> Result<Self> {
        let service_name = wide_string(service_name);
        let host_name = wide_string(host_name);
        let keys: Vec<Vec<u16>> = properties.iter().map(|(key, _)| wide_string(key)).collect();
        let values: Vec<Vec<u16>> = properties
            .iter()
            .map(|(_, value)| wide_string(value))
            .collect();
        let key_pointers: Vec<*const u16> = keys.iter().map(|value| value.as_ptr()).collect();
        let value_pointers: Vec<*const u16> = values.iter().map(|value| value.as_ptr()).collect();

        // DNS_SERVICE_INSTANCE stores IP4_ADDRESS in network byte order.  A
        // native-endian integer built from the address octets has exactly the
        // required in-memory byte layout on Windows.  Supplying this A record
        // is essential for third-party clients: passing NULL appears to work
        // while Xiaomi's MAFSvr is installed because its advertisement can
        // supply the host address, but the standalone FusionPlay service then
        // becomes unresolvable on a clean machine.
        let ip4_address = u32::from_ne_bytes(local_ip.octets());

        // SAFETY: all UTF-16 buffers, the IPv4 value and pointer arrays remain
        // alive for DnsServiceConstructInstance, which copies their data.
        let instance = unsafe {
            DnsServiceConstructInstance(
                service_name.as_ptr(),
                host_name.as_ptr(),
                &ip4_address,
                std::ptr::null(),
                port,
                0,
                0,
                u32::try_from(properties.len()).unwrap_or(u32::MAX),
                key_pointers.as_ptr(),
                value_pointers.as_ptr(),
            )
        };
        if instance.is_null() {
            return Err(anyhow::anyhow!("DnsServiceConstructInstance returned null"));
        }

        let mut callback_context = Box::new(DnsServiceCallbackContext {
            sender: Mutex::new(None),
        });
        let mut request = Box::new(DnsServiceRegisterRequest {
            version: DNS_QUERY_REQUEST_VERSION1,
            interface_index,
            service_instance: instance,
            completion_callback: Some(dns_service_complete),
            query_context: (&mut *callback_context as *mut DnsServiceCallbackContext).cast(),
            credentials: std::ptr::null_mut(),
            unicast_enabled: 0,
        });
        let (sender, receiver) = mpsc::sync_channel(1);
        *callback_context
            .sender
            .lock()
            .map_err(|_| anyhow::anyhow!("DNS-SD callback lock poisoned"))? = Some(sender);

        // SAFETY: request, instance and callback_context are retained in Self.
        let status = unsafe { DnsServiceRegister(&mut *request, std::ptr::null_mut()) };
        if status != DNS_REQUEST_PENDING && status != 0 {
            unsafe { DnsServiceFreeInstance(instance) };
            return Err(anyhow::anyhow!(
                "DnsServiceRegister failed with status {status}"
            ));
        }
        if status == DNS_REQUEST_PENDING
            && let Ok(completion_status) = receiver.recv_timeout(Duration::from_secs(5))
            && completion_status != 0
        {
            unsafe { DnsServiceFreeInstance(instance) };
            return Err(anyhow::anyhow!(
                "DNS-SD registration callback failed with status {completion_status}"
            ));
        }

        Ok(Self {
            instance,
            request: Some(request),
            callback_context: Some(callback_context),
        })
    }
}

#[cfg(windows)]
impl Drop for WindowsDnsSdService {
    fn drop(&mut self) {
        let Some(request) = self.request.as_mut() else {
            return;
        };
        let Some(context) = self.callback_context.as_mut() else {
            return;
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        if let Ok(mut pending_sender) = context.sender.lock() {
            *pending_sender = Some(sender);
        }
        // SAFETY: the same live request used for registration is retained here.
        let status = unsafe { DnsServiceDeRegister(&mut **request, std::ptr::null_mut()) };
        let completed = status == 0
            || (status == DNS_REQUEST_PENDING
                && receiver.recv_timeout(Duration::from_secs(3)).is_ok());
        if completed {
            unsafe { DnsServiceFreeInstance(self.instance) };
            self.instance = std::ptr::null_mut();
        } else {
            // Keep asynchronous callback storage alive rather than risking a
            // use-after-free during abnormal Windows DNS service shutdown.
            if let Some(request) = self.request.take() {
                std::mem::forget(request);
            }
            if let Some(context) = self.callback_context.take() {
                std::mem::forget(context);
            }
        }
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn spawn_windows_dnssd_responder(
    receiver_name: String,
    lyra_instance_name: String,
    mi_connect_instance_name: String,
    idm_instance_id: String,
    media_device_id: String,
    device_type: MiPlayDeviceType,
    local_ip: Ipv4Addr,
    link_local_v6: Option<Ipv6Addr>,
    mi_connect_ipv6: Option<Ipv6Addr>,
    interface_name: String,
    diagnostic_multicast_loopback: bool,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) -> Result<JoinHandle<()>> {
    let interface_index = if_addrs::get_if_addrs()
        .ok()
        .and_then(|interfaces| {
            interfaces
                .into_iter()
                .find(|interface| interface.ip() == IpAddr::V4(local_ip))
                .and_then(|interface| interface.index)
        })
        .unwrap_or(0);
    let (startup_sender, startup_receiver) = mpsc::sync_channel::<Result<()>>(1);
    let worker = thread::Builder::new()
        .name("miplay-windows-dnssd".to_owned())
        .spawn(move || {
            let lyra_response = build_lyra_response(
                &receiver_name,
                &lyra_instance_name,
                &media_device_id,
                device_type,
                local_ip,
                link_local_v6,
                &interface_name,
            );
            let mi_connect_response = build_mi_connect_response(
                &receiver_name,
                &mi_connect_instance_name,
                &idm_instance_id,
                &media_device_id,
                device_type,
                local_ip,
                mi_connect_ipv6,
            );
            let lyra_properties = parse_txt_properties(&build_txt_data(
                &receiver_name,
                &lyra_instance_name,
                &media_device_id,
                device_type,
                local_ip,
                link_local_v6,
                &interface_name,
            ));
            let mi_connect_properties = parse_txt_properties(&build_mi_connect_txt(
                &receiver_name,
                &idm_instance_id,
                &media_device_id,
                device_type,
            ));
            let lyra_mdns_host_name = format!("{lyra_instance_name}.local");
            let mi_connect_mdns_host_name = format!("{mi_connect_instance_name}.local");
            let registration = if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                (|| -> Result<Vec<WindowsDnsSdService>> {
                    let lyra_service_name = format!("{lyra_instance_name}.{LYRA_SERVICE_TYPE}");
                    let mi_connect_service_name =
                        format!("{mi_connect_instance_name}.{MI_CONNECT_SERVICE_TYPE}");
                    // MiPCAudio uses the Xiaomi instance name as the SRV target.
                    // Some senders display an entry whose target is the Windows
                    // computer name but refuse to start the TCP control session.
                    // Keep the official target here; the exact UDP responder
                    // below supplies the A/AAAA records that DnsServiceRegister
                    // omits for custom host names.
                    let lyra = WindowsDnsSdService::register(
                        &lyra_service_name,
                        &lyra_mdns_host_name,
                        LYRA_CONTROL_PORT,
                        interface_index,
                        local_ip,
                        &lyra_properties,
                    )?;
                    let mi_connect = WindowsDnsSdService::register(
                        &mi_connect_service_name,
                        &mi_connect_mdns_host_name,
                        MI_CONNECT_DISCOVERY_PORT,
                        interface_index,
                        local_ip,
                        &mi_connect_properties,
                    )?;
                    Ok(vec![lyra, mi_connect])
                })()
            } else {
                Ok(Vec::new())
            };

            match registration {
                Ok(registrations) => {
                    events(json!({
                        "event": "discovery_backend_ready",
                        "protocol": "xiaomi_miplay",
                        "backend": if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                            "windows_dns_sd_plus_exact_udp"
                        } else {
                            "exact_udp"
                        },
                        "interface_index": interface_index,
                        "address": local_ip.to_string(),
                        "address_resolution_host": lyra_mdns_host_name,
                        "address_resolution_strategy": "single_authoritative_xiaomi_wire_response",
                        "windows_dns_sd_registration": ENABLE_WINDOWS_DNS_SD_REGISTRATION,
                        "external_service_required": false,
                    }));
                    if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                        for service in [LYRA_SERVICE_TYPE, MI_CONNECT_SERVICE_TYPE] {
                            events(json!({
                                "event": "discovery_announcement",
                                "protocol": "xiaomi_miplay",
                                "service": service,
                                "backend": "windows_dns_sd",
                                "success": true,
                            }));
                        }
                    }
                    match create_responder_socket(local_ip, diagnostic_multicast_loopback) {
                        Ok(socket) => {
                            let responses = Arc::new(DiscoveryResponses {
                                lyra: lyra_response,
                                mi_connect: mi_connect_response,
                                lyra_instance_name: lyra_instance_name.clone(),
                                mi_connect_instance_name: mi_connect_instance_name.clone(),
                                retired_mi_connect_goodbyes: Vec::new(),
                            });
                            events(json!({
                                "event": "discovery_exact_responder_ready",
                                "protocol": "xiaomi_miplay",
                                "backend": if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                                    "windows_dns_sd_plus_exact_udp"
                                } else {
                                    "exact_udp"
                                },
                                "address": local_ip.to_string(),
                                "port": MDNS_PORT,
                                "diagnostic_multicast_loopback": diagnostic_multicast_loopback,
                                "external_service_required": false,
                            }));

                            // Xiaomi discovery is reciprocal. MAFSvr browses
                            // the sender's Lyra record from an ephemeral port
                            // while MiPCAudio answers the sender's own browse
                            // on port 5353. Keep these sockets separate so the
                            // wire flow and QU reply destination match the
                            // official implementation exactly.
                            let peer_browser_ipv4 = match create_ipv4_browser_socket(local_ip) {
                                Ok(browser_socket) => match spawn_lyra_peer_browser(
                                    browser_socket,
                                    "ipv4",
                                    SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT)),
                                    Arc::clone(&shutdown),
                                    Arc::clone(&events),
                                ) {
                                    Ok(worker) => Some(worker),
                                    Err(error) => {
                                        events(json!({
                                            "event": "discovery_peer_browser_failed",
                                            "protocol": "xiaomi_miplay",
                                            "address_family": "ipv4",
                                            "stage": "spawn",
                                            "message": format!("{error:#}"),
                                        }));
                                        None
                                    }
                                },
                                Err(error) => {
                                    events(json!({
                                        "event": "discovery_peer_browser_failed",
                                        "protocol": "xiaomi_miplay",
                                        "address_family": "ipv4",
                                        "stage": "socket",
                                        "message": format!("{error:#}"),
                                    }));
                                    None
                                }
                            };

                            let peer_browser_ipv6 = if let Some(link_local_ip) = link_local_v6 {
                                match create_ipv6_browser_socket(interface_index, link_local_ip) {
                                    Ok(browser_socket) => match spawn_lyra_peer_browser(
                                        browser_socket,
                                        "ipv6",
                                        SocketAddr::V6(SocketAddrV6::new(
                                            MDNS_GROUP_V6,
                                            MDNS_PORT,
                                            0,
                                            interface_index,
                                        )),
                                        Arc::clone(&shutdown),
                                        Arc::clone(&events),
                                    ) {
                                        Ok(worker) => Some(worker),
                                        Err(error) => {
                                            events(json!({
                                                "event": "discovery_peer_browser_failed",
                                                "protocol": "xiaomi_miplay",
                                                "address_family": "ipv6",
                                                "stage": "spawn",
                                                "message": format!("{error:#}"),
                                            }));
                                            None
                                        }
                                    },
                                    Err(error) => {
                                        events(json!({
                                            "event": "discovery_peer_browser_failed",
                                            "protocol": "xiaomi_miplay",
                                            "address_family": "ipv6",
                                            "stage": "socket",
                                            "message": format!("{error:#}"),
                                        }));
                                        None
                                    }
                                }
                            } else {
                                events(json!({
                                    "event": "discovery_peer_browser_skipped",
                                    "protocol": "xiaomi_miplay",
                                    "address_family": "ipv6",
                                    "reason": "link_local_address_unavailable",
                                }));
                                None
                            };

                            // MiPCAudio joins ff02::fb on the selected adapter and
                            // answers every Lyra/MiConnect browse over both IP
                            // families. HyperOS keeps one shared discovery cache;
                            // an IPv4-only result can be discarded when the phone
                            // has already issued the same browse on IPv6. Keep the
                            // IPv4 responder authoritative and add the matching
                            // link-local IPv6 listener without introducing a
                            // dependency on Windows DNS-SD or Xiaomi services.
                            let ipv6_worker = if interface_index == 0 {
                                events(json!({
                                    "event": "discovery_ipv6_responder_skipped",
                                    "protocol": "xiaomi_miplay",
                                    "reason": "interface_index_unavailable",
                                }));
                                None
                            } else {
                                match create_ipv6_responder_socket(
                                    interface_index,
                                    diagnostic_multicast_loopback,
                                ) {
                                    Ok(ipv6_socket) => {
                                        let ipv6_address = link_local_v6
                                            .or(mi_connect_ipv6)
                                            .unwrap_or(Ipv6Addr::UNSPECIFIED);
                                        events(json!({
                                            "event": "discovery_ipv6_responder_ready",
                                            "protocol": "xiaomi_miplay",
                                            "address": ipv6_address.to_string(),
                                            "port": MDNS_PORT,
                                            "interface_index": interface_index,
                                            "multicast_group": MDNS_GROUP_V6.to_string(),
                                            "external_service_required": false,
                                        }));
                                        let ipv6_responses = Arc::clone(&responses);
                                        let ipv6_shutdown = Arc::clone(&shutdown);
                                        let ipv6_events = Arc::clone(&events);
                                        match thread::Builder::new()
                                            .name("miplay-ipv6-dnssd".to_owned())
                                            .spawn(move || {
                                                responder_loop_ipv6(
                                                    ipv6_socket,
                                                    interface_index,
                                                    ipv6_address,
                                                    ipv6_responses,
                                                    ipv6_shutdown,
                                                    ipv6_events,
                                                );
                                            }) {
                                            Ok(worker) => Some(worker),
                                            Err(error) => {
                                                events(json!({
                                                    "event": "discovery_ipv6_responder_failed",
                                                    "protocol": "xiaomi_miplay",
                                                    "stage": "spawn",
                                                    "error": error.to_string(),
                                                    "ipv4_fallback_active": true,
                                                }));
                                                None
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        events(json!({
                                            "event": "discovery_ipv6_responder_failed",
                                            "protocol": "xiaomi_miplay",
                                            "stage": "socket",
                                            "error": format!("{error:#}"),
                                            "interface_index": interface_index,
                                            "ipv4_fallback_active": true,
                                        }));
                                        None
                                    }
                                }
                            };
                            let _ = startup_sender.send(Ok(()));
                            responder_loop(
                                socket,
                                local_ip,
                                responses,
                                Arc::clone(&shutdown),
                                Arc::clone(&events),
                            );
                            if let Some(worker) = ipv6_worker {
                                let _ = worker.join();
                            }
                            if let Some(worker) = peer_browser_ipv4 {
                                let _ = worker.join();
                            }
                            if let Some(worker) = peer_browser_ipv6 {
                                let _ = worker.join();
                            }
                        }
                        Err(error) => {
                            let message = format!("{error:#}");
                            events(json!({
                                "event": "discovery_exact_responder_failed",
                                "protocol": "xiaomi_miplay",
                                "backend": "exact_udp",
                                "error": message,
                                "fallback": if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                                    "windows_dns_sd"
                                } else {
                                    "none"
                                },
                            }));
                            if ENABLE_WINDOWS_DNS_SD_REGISTRATION {
                                let _ = startup_sender.send(Ok(()));
                                while !shutdown.load(Ordering::Acquire) {
                                    thread::sleep(Duration::from_millis(100));
                                }
                            } else {
                                let _ = startup_sender.send(Err(anyhow::anyhow!(message)));
                            }
                        }
                    }
                    drop(registrations);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let _ = startup_sender.send(Err(anyhow::anyhow!(message.clone())));
                    events(json!({
                        "event": "discovery_backend_failed",
                        "protocol": "xiaomi_miplay",
                        "backend": "windows_dns_sd",
                        "error": message,
                    }));
                }
            }
        })
        .context("spawn Windows DNS-SD discovery responder")?;

    match startup_receiver.recv_timeout(Duration::from_secs(12)) {
        Ok(Ok(())) => Ok(worker),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }
        Err(error) => Err(anyhow::anyhow!(
            "Windows DNS-SD discovery startup timed out: {error}"
        )),
    }
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn parse_txt_properties(txt: &[u8]) -> Vec<(String, String)> {
    let mut properties = Vec::new();
    let mut offset = 0usize;
    while offset < txt.len() {
        let length = usize::from(txt[offset]);
        offset += 1;
        if offset + length > txt.len() {
            break;
        }
        if let Ok(entry) = std::str::from_utf8(&txt[offset..offset + length])
            && let Some((key, value)) = entry.split_once('=')
        {
            properties.push((key.to_owned(), value.to_owned()));
        }
        offset += length;
    }
    properties
}

fn create_responder_socket(
    local_ip: Ipv4Addr,
    diagnostic_multicast_loopback: bool,
) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("create Lyra discovery socket")?;
    socket
        .set_reuse_address(true)
        .context("enable Lyra discovery socket reuse")?;
    // mDNS questions are addressed to 224.0.0.251, not to the adapter's
    // unicast address.  Binding the receive socket to `local_ip` makes local
    // unicast probes appear healthy while external multicast questions can
    // be filtered out before recv_from.  Bind the wildcard address and use
    // IP_MULTICAST_IF plus group membership below to pin traffic to the
    // selected physical adapter on every supported platform.
    let bind_ip = Ipv4Addr::UNSPECIFIED;
    socket
        .bind(&SocketAddrV4::new(bind_ip, MDNS_PORT).into())
        .with_context(|| format!("bind MiPlay discovery socket to {bind_ip}:{MDNS_PORT}"))?;
    socket
        .set_multicast_if_v4(&local_ip)
        .context("select Lyra discovery network adapter")?;
    socket
        .set_multicast_ttl_v4(255)
        .context("set Lyra multicast TTL")?;
    // Xiaomi sends a QU browse from an ephemeral UDP port, so the exact
    // Lyra answer is unicast even though discovery itself is mDNS. RFC 6762
    // requires an IP TTL of 255 for received mDNS traffic, and HyperOS drops
    // Windows' default-TTL (128) unicast reply before it reaches the picker.
    socket.set_ttl_v4(255).context("set Lyra unicast TTL")?;
    socket
        .set_multicast_loop_v4(diagnostic_multicast_loopback)
        .context("configure Lyra multicast loopback")?;
    socket
        .set_broadcast(true)
        .context("enable Lyra wired discovery broadcast fallback")?;

    let socket: UdpSocket = socket.into();
    socket
        .join_multicast_v4(&MDNS_GROUP, &local_ip)
        .context("join Lyra mDNS multicast group")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set Lyra discovery receive timeout")?;
    Ok(socket)
}

#[cfg(windows)]
fn create_ipv6_responder_socket(
    interface_index: u32,
    diagnostic_multicast_loopback: bool,
) -> Result<UdpSocket> {
    if interface_index == 0 {
        anyhow::bail!("selected MiPlay interface has no IPv6 scope index");
    }

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .context("create IPv6 Lyra discovery socket")?;
    socket
        .set_only_v6(true)
        .context("restrict Lyra IPv6 socket to IPv6")?;
    socket
        .set_reuse_address(true)
        .context("enable IPv6 Lyra discovery socket reuse")?;
    let bind_address = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, MDNS_PORT, 0, 0);
    socket
        .bind(&bind_address.into())
        .with_context(|| format!("bind IPv6 MiPlay discovery socket to {bind_address}"))?;
    socket
        .set_multicast_if_v6(interface_index)
        .context("select IPv6 Lyra discovery network adapter")?;
    socket
        .set_multicast_hops_v6(255)
        .context("set IPv6 Lyra multicast hop limit")?;
    socket
        .set_unicast_hops_v6(255)
        .context("set IPv6 Lyra unicast hop limit")?;
    socket
        .set_multicast_loop_v6(diagnostic_multicast_loopback)
        .context("configure IPv6 Lyra multicast loopback")?;

    let socket: UdpSocket = socket.into();
    socket
        .join_multicast_v6(&MDNS_GROUP_V6, interface_index)
        .context("join IPv6 Lyra mDNS multicast group")?;
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set IPv6 Lyra discovery receive timeout")?;
    Ok(socket)
}

fn build_lyra_browse_query() -> Vec<u8> {
    let mut query = Vec::with_capacity(39);
    // Standard mDNS query header with one question and no known answers.
    query.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    query.extend_from_slice(LYRA_SERVICE_QNAME);
    // PTR + QU bit. Xiaomi's MAFSvr uses this exact question so peers return
    // their complete Lyra record directly to our ephemeral source port.
    query.extend_from_slice(&12_u16.to_be_bytes());
    query.extend_from_slice(&0x8001_u16.to_be_bytes());
    query
}

fn create_ipv4_browser_socket(local_ip: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("create IPv4 Lyra peer browser socket")?;
    socket
        .bind(&SocketAddrV4::new(local_ip, 0).into())
        .with_context(|| format!("bind IPv4 Lyra peer browser to {local_ip}"))?;
    socket
        .set_multicast_if_v4(&local_ip)
        .context("select IPv4 Lyra peer browser adapter")?;
    socket
        .set_multicast_ttl_v4(255)
        .context("set IPv4 Lyra peer browser multicast TTL")?;
    socket
        .set_ttl_v4(255)
        .context("set IPv4 Lyra peer browser unicast TTL")?;
    socket
        .set_multicast_loop_v4(false)
        .context("disable IPv4 Lyra peer browser loopback")?;
    let socket: UdpSocket = socket.into();
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set IPv4 Lyra peer browser receive timeout")?;
    Ok(socket)
}

fn create_ipv6_browser_socket(interface_index: u32, link_local_ip: Ipv6Addr) -> Result<UdpSocket> {
    if interface_index == 0 || !link_local_ip.is_unicast_link_local() {
        anyhow::bail!("selected MiPlay interface has no usable IPv6 link-local address");
    }
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .context("create IPv6 Lyra peer browser socket")?;
    socket
        .set_only_v6(true)
        .context("restrict IPv6 Lyra peer browser to IPv6")?;
    let bind_address = SocketAddrV6::new(link_local_ip, 0, 0, interface_index);
    socket
        .bind(&bind_address.into())
        .with_context(|| format!("bind IPv6 Lyra peer browser to {bind_address}"))?;
    socket
        .set_multicast_if_v6(interface_index)
        .context("select IPv6 Lyra peer browser adapter")?;
    socket
        .set_multicast_hops_v6(255)
        .context("set IPv6 Lyra peer browser multicast hop limit")?;
    socket
        .set_unicast_hops_v6(255)
        .context("set IPv6 Lyra peer browser unicast hop limit")?;
    socket
        .set_multicast_loop_v6(false)
        .context("disable IPv6 Lyra peer browser loopback")?;
    let socket: UdpSocket = socket.into();
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("set IPv6 Lyra peer browser receive timeout")?;
    Ok(socket)
}

fn spawn_lyra_peer_browser(
    socket: UdpSocket,
    address_family: &'static str,
    multicast: SocketAddr,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("miplay-{address_family}-peer-browser"))
        .spawn(move || {
            let query = build_lyra_browse_query();
            let local_address = socket
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_else(|_| "unknown".to_owned());
            let mut round = 0_u64;
            let mut next_query = Instant::now();
            let mut buffer = [0_u8; 4096];
            while !shutdown.load(Ordering::Acquire) {
                if Instant::now() >= next_query {
                    round += 1;
                    let result = socket.send_to(&query, multicast);
                    events(json!({
                        "event": "discovery_peer_query",
                        "protocol": "xiaomi_miplay",
                        "service": LYRA_SERVICE_TYPE,
                        "address_family": address_family,
                        "source": local_address,
                        "destination": multicast.to_string(),
                        "bytes": query.len(),
                        "wire_hex": hex::encode(&query),
                        "round": round,
                        "reason": if round <= 3 { "startup_burst" } else { "cache_refresh" },
                        "success": result.is_ok(),
                        "error": result.err().map(|error| error.to_string()),
                    }));
                    next_query = Instant::now()
                        + if round < 3 {
                            Duration::from_secs(1)
                        } else {
                            // This matches the refresh cadence observed in
                            // Xiaomi's MAFSvr rather than continuously polling.
                            Duration::from_secs(300)
                        };
                }

                match socket.recv_from(&mut buffer) {
                    Ok((length, source)) => {
                        let packet = &buffer[..length];
                        let is_response = length >= 12 && packet[2] & 0x80 != 0;
                        let is_lyra = packet
                            .windows(LYRA_SERVICE_QNAME.len())
                            .any(|window| window == LYRA_SERVICE_QNAME);
                        events(json!({
                            "event": if is_response && is_lyra {
                                "discovery_peer_found"
                            } else {
                                "discovery_peer_packet_ignored"
                            },
                            "protocol": "xiaomi_miplay",
                            "service": if is_lyra { Some(LYRA_SERVICE_TYPE) } else { None },
                            "address_family": address_family,
                            "source": source.to_string(),
                            "destination": local_address,
                            "bytes": length,
                            "wire_hex": hex::encode(packet),
                        }));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => {
                        events(json!({
                            "event": "discovery_peer_browser_failed",
                            "protocol": "xiaomi_miplay",
                            "address_family": address_family,
                            "source": local_address,
                            "message": error.to_string(),
                        }));
                        break;
                    }
                }
            }
        })
        .context("spawn Lyra peer browser")
}

fn responder_loop(
    socket: UdpSocket,
    local_ip: Ipv4Addr,
    responses: Arc<DiscoveryResponses>,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) {
    let multicast = SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT));
    // A number of consumer routers isolate Wi-Fi multicast from their wired
    // bridge while still forwarding ordinary L2 broadcast traffic.  Xiaomi's
    // sender then emits the correct mDNS query, but a receiver connected by
    // Ethernet never sees it.  Keep standards-compliant mDNS as the primary
    // path and mirror only the small periodic announcement onto the local
    // limited-broadcast address.  This stays on the LAN and does not require
    // a Xiaomi background service, router setting or fixed subnet mask.
    let wired_broadcast_address = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .find_map(|interface| match interface.addr {
            if_addrs::IfAddr::V4(address) if address.ip == local_ip => address.broadcast,
            _ => None,
        })
        .unwrap_or(Ipv4Addr::new(255, 255, 255, 255));
    let wired_broadcast = SocketAddr::V4(SocketAddrV4::new(wired_broadcast_address, MDNS_PORT));

    responder_loop_inner(
        &socket,
        "ipv4",
        local_ip.to_string(),
        multicast,
        Some(wired_broadcast),
        responses,
        shutdown,
        events,
    );

    let _ = socket.leave_multicast_v4(&MDNS_GROUP, &local_ip);
}

#[cfg(windows)]
fn responder_loop_ipv6(
    socket: UdpSocket,
    interface_index: u32,
    local_ip: Ipv6Addr,
    responses: Arc<DiscoveryResponses>,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) {
    let multicast = SocketAddr::V6(SocketAddrV6::new(
        MDNS_GROUP_V6,
        MDNS_PORT,
        0,
        interface_index,
    ));
    responder_loop_inner(
        &socket,
        "ipv6",
        local_ip.to_string(),
        multicast,
        None,
        responses,
        shutdown,
        events,
    );

    let _ = socket.leave_multicast_v6(&MDNS_GROUP_V6, interface_index);
}

#[allow(clippy::too_many_arguments)]
fn responder_loop_inner(
    socket: &UdpSocket,
    address_family: &str,
    local_address: String,
    multicast: SocketAddr,
    wired_broadcast: Option<SocketAddr>,
    responses: Arc<DiscoveryResponses>,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) {
    // Keep the desktop receiver's paired identity rule on every platform.
    // Both records are query-scoped in production so their first appearance
    // falls inside the same MiPlay scan window. The diagnostic environment
    // override is the only mode that restores periodic publication.
    let persist_generic_tv_discovery = persistent_mi_connect_discovery(
        cfg!(target_os = "android"),
        std::env::var("FUSIONPLAY_MIPLAY_ENABLE_GENERIC_TV_DISCOVERY")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "TRUE")),
    );
    events(json!({
        "event": "discovery_publication_profile",
        "protocol": "xiaomi_miplay",
        "address_family": address_family,
        "profile": if persist_generic_tv_discovery && APP_LYRA_DISCOVERY_ENABLED {
            "persistent_mi_connect_lyra_route"
        } else if persist_generic_tv_discovery {
            "persistent_mi_connect_route"
        } else if APP_LYRA_DISCOVERY_ENABLED {
            "on_demand_mi_connect_lyra_route"
        } else {
            "on_demand_mi_connect_route"
        },
        "lyra_advertisement_enabled": persist_generic_tv_discovery
            && APP_LYRA_DISCOVERY_ENABLED,
        "lyra_query_responses_enabled": APP_LYRA_DISCOVERY_ENABLED,
        "mi_connect_query_responses_enabled": true,
        "mi_connect_periodic_advertisement_enabled": persist_generic_tv_discovery,
        "mi_connect_ttl_seconds": if persist_generic_tv_discovery {
            LYRA_TTL_SECONDS
        } else {
            ON_DEMAND_MI_CONNECT_TTL_SECONDS
        },
        "fusion_device_center_tv_persistent": persist_generic_tv_discovery,
        "account_required": false,
    }));

    // Retire persistent identities emitted by earlier builds so a warm
    // HyperOS cache cannot keep a persistent route beside the canonical
    // short-lived pair. The same responses are subsequently used only for
    // active queries with a ten-second TTL.
    if !persist_generic_tv_discovery || !APP_LYRA_DISCOVERY_ENABLED {
        let mut retired_records = vec![(
            LYRA_SERVICE_TYPE.to_owned(),
            zero_dns_record_ttls(&responses.lyra).unwrap_or_else(|| responses.lyra.clone()),
        )];
        if !persist_generic_tv_discovery {
            retired_records.push((
                MI_CONNECT_SERVICE_TYPE.to_owned(),
                zero_dns_record_ttls(&responses.mi_connect)
                    .unwrap_or_else(|| responses.mi_connect.clone()),
            ));
        }
        for retired_instance in RETIRED_LYRA_INSTANCE_NAMES {
            if let Some(goodbye) = build_retired_dns_sd_goodbye(
                &responses.lyra,
                &responses.lyra_instance_name,
                retired_instance,
            ) {
                retired_records.push((LYRA_SERVICE_TYPE.to_owned(), goodbye));
            }
        }
        retired_records.extend(
            responses
                .retired_mi_connect_goodbyes
                .iter()
                .cloned()
                .map(|goodbye| (MI_CONNECT_SERVICE_TYPE.to_owned(), goodbye)),
        );
        send_discovery_goodbye_burst(
            socket,
            address_family,
            multicast,
            wired_broadcast,
            &retired_records,
            "persistent_route_retirement",
            &events,
        );
    }

    let on_demand_mi_connect_response =
        set_dns_record_ttls(&responses.mi_connect, ON_DEMAND_MI_CONNECT_TTL_SECONDS)
            .unwrap_or_else(|| responses.mi_connect.clone());
    let on_demand_lyra_response =
        set_dns_record_ttls(&responses.lyra, ON_DEMAND_MI_CONNECT_TTL_SECONDS)
            .unwrap_or_else(|| responses.lyra.clone());
    let mut next_announcement = persist_generic_tv_discovery.then(Instant::now);
    let mut announced_once = false;
    let mut last_on_demand_query: Option<Instant> = None;
    let mut buffer = [0u8; 4096];
    while !shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        if next_announcement.is_some_and(|deadline| now >= deadline) {
            let mut announcements = vec![(
                MI_CONNECT_SERVICE_TYPE.to_owned(),
                responses.mi_connect.clone(),
            )];
            if APP_LYRA_DISCOVERY_ENABLED {
                announcements.insert(0, (LYRA_SERVICE_TYPE.to_owned(), responses.lyra.clone()));
            }
            send_discovery_announcement_burst(
                socket,
                address_family,
                multicast,
                wired_broadcast,
                &announcements,
                if announced_once {
                    "ttl_refresh"
                } else {
                    "startup"
                },
                &events,
            );
            announced_once = true;
            next_announcement = Some(Instant::now() + DISCOVERY_ANNOUNCEMENT_INTERVAL);
        }
        if !persist_generic_tv_discovery
            && last_on_demand_query.is_some_and(|last_query| {
                now.duration_since(last_query) >= ON_DEMAND_MI_CONNECT_IDLE_TIMEOUT
            })
        {
            let mut expired_route = vec![(
                MI_CONNECT_SERVICE_TYPE.to_owned(),
                zero_dns_record_ttls(&on_demand_mi_connect_response)
                    .unwrap_or_else(|| on_demand_mi_connect_response.clone()),
            )];
            if APP_LYRA_DISCOVERY_ENABLED {
                expired_route.insert(
                    0,
                    (
                        LYRA_SERVICE_TYPE.to_owned(),
                        zero_dns_record_ttls(&on_demand_lyra_response)
                            .unwrap_or_else(|| on_demand_lyra_response.clone()),
                    ),
                );
            }
            send_discovery_goodbye_burst(
                socket,
                address_family,
                multicast,
                wired_broadcast,
                &expired_route,
                "on_demand_scan_expired",
                &events,
            );
            last_on_demand_query = None;
        }
        match socket.recv_from(&mut buffer) {
            Ok((length, source)) => {
                let query = &buffer[..length];
                events(json!({
                    "event": "discovery_packet_received",
                    "protocol": "xiaomi_miplay",
                    "address_family": address_family,
                    "source": source.to_string(),
                    "bytes": length,
                    "wire_hex": hex::encode(query),
                }));
                let parsed_queries = parse_dns_questions(query);
                let requests_unicast = parsed_queries
                    .iter()
                    .any(|(_, _, qclass)| qclass & 0x8000 != 0);
                let destination = if source.port() != MDNS_PORT || requests_unicast {
                    source
                } else {
                    multicast
                };
                let should_reply_lyra = APP_LYRA_DISCOVERY_ENABLED
                    && parsed_queries.iter().any(|(qname, qtype, _)| {
                        query_matches_service(
                            qname,
                            *qtype,
                            LYRA_SERVICE_NAME,
                            &responses.lyra_instance_name,
                        )
                    });
                let should_reply_mi_connect = parsed_queries.iter().any(|(qname, qtype, _)| {
                    query_matches_service(
                        qname,
                        *qtype,
                        MI_CONNECT_SERVICE_NAME,
                        &responses.mi_connect_instance_name,
                    )
                });
                if should_reply_lyra {
                    let response = if persist_generic_tv_discovery {
                        &responses.lyra
                    } else {
                        last_on_demand_query = Some(Instant::now());
                        &on_demand_lyra_response
                    };
                    send_discovery_reply(
                        socket,
                        query,
                        source,
                        destination,
                        LYRA_SERVICE_TYPE,
                        response,
                        &events,
                    );
                }
                if should_reply_mi_connect {
                    let response = if persist_generic_tv_discovery {
                        &responses.mi_connect
                    } else {
                        last_on_demand_query = Some(Instant::now());
                        &on_demand_mi_connect_response
                    };
                    send_discovery_reply(
                        socket,
                        query,
                        source,
                        destination,
                        MI_CONNECT_SERVICE_TYPE,
                        response,
                        &events,
                    );
                }
                if !should_reply_lyra && !should_reply_mi_connect && !parsed_queries.is_empty() {
                    events(json!({
                        "event": "discovery_query_unsupported",
                        "protocol": "xiaomi_miplay",
                        "address_family": address_family,
                        "source": source.to_string(),
                        "bytes": length,
                        "wire_hex": hex::encode(query),
                        "query": parsed_queries
                            .into_iter()
                            .map(|(name, qtype, qclass)| format!("{name}/{qtype}/{qclass:#06x}"))
                            .collect::<Vec<_>>(),
                    }));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                events(json!({
                    "event": "discovery_receive_failed",
                    "protocol": "xiaomi_miplay",
                    "address_family": address_family,
                    "address": local_address,
                    "port": MDNS_PORT,
                    "message": error.to_string(),
                }));
                break;
            }
        }
    }

    let mut shutdown_goodbyes = vec![(
        MI_CONNECT_SERVICE_TYPE.to_owned(),
        zero_dns_record_ttls(&responses.mi_connect).unwrap_or_else(|| responses.mi_connect.clone()),
    )];
    if APP_LYRA_DISCOVERY_ENABLED {
        shutdown_goodbyes.insert(
            0,
            (
                LYRA_SERVICE_TYPE.to_owned(),
                zero_dns_record_ttls(&responses.lyra).unwrap_or_else(|| responses.lyra.clone()),
            ),
        );
    }
    send_discovery_goodbye_burst(
        socket,
        address_family,
        multicast,
        wired_broadcast,
        &shutdown_goodbyes,
        "shutdown",
        &events,
    );
}

fn send_discovery_announcement_burst(
    socket: &UdpSocket,
    address_family: &str,
    multicast: SocketAddr,
    wired_broadcast: Option<SocketAddr>,
    announcements: &[(String, Vec<u8>)],
    reason: &str,
    events: &EventEmitter,
) {
    // DNS-SD registration sends a short startup burst so a listener that is
    // joining the multicast group at the same time does not miss the only
    // record capable of rebuilding an empty cache.  Periodic TTL refreshes do
    // not need that redundancy.
    let rounds = if reason == "startup" { 3 } else { 1 };
    for round in 1..=rounds {
        for (service, payload) in announcements {
            let multicast_result = socket.send_to(payload, multicast);
            let wired_broadcast_success =
                wired_broadcast.map(|destination| socket.send_to(payload, destination).is_ok());
            events(json!({
                "event": "discovery_announcement",
                "protocol": "xiaomi_miplay",
                "service": service,
                "address_family": address_family,
                "destination": multicast.to_string(),
                "bytes": payload.len(),
                "round": round,
                "reason": reason,
                "success": multicast_result.is_ok(),
                "wired_broadcast_success": wired_broadcast_success,
            }));
        }
        if round < rounds {
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn send_discovery_goodbye_burst(
    socket: &UdpSocket,
    address_family: &str,
    multicast: SocketAddr,
    wired_broadcast: Option<SocketAddr>,
    goodbyes: &[(String, Vec<u8>)],
    reason: &str,
    events: &EventEmitter,
) {
    if goodbyes.is_empty() {
        return;
    }
    // A listener restart already serializes teardown before the replacement
    // socket can answer queries. Keep all three cache-eviction packets, but do
    // not hold the discovery socket offline for another 500 ms during restart.
    // Scan-expiry goodbyes retain their spacing because no replacement route
    // follows them immediately.
    let repeat_interval = discovery_goodbye_repeat_interval(reason);
    for round in 1..=3 {
        for (service, payload) in goodbyes {
            let multicast_result = socket.send_to(payload, multicast);
            let wired_broadcast_success =
                wired_broadcast.map(|destination| socket.send_to(payload, destination).is_ok());
            events(json!({
                "event": "discovery_goodbye",
                "protocol": "xiaomi_miplay",
                "service": service,
                "address_family": address_family,
                "destination": multicast.to_string(),
                "bytes": payload.len(),
                "round": round,
                "reason": reason,
                "success": multicast_result.is_ok(),
                "wired_broadcast_success": wired_broadcast_success,
            }));
        }
        if round < 3 && !repeat_interval.is_zero() {
            thread::sleep(repeat_interval);
        }
    }
}

fn discovery_goodbye_repeat_interval(reason: &str) -> Duration {
    match reason {
        "shutdown" | "persistent_route_retirement" => Duration::ZERO,
        _ => Duration::from_millis(250),
    }
}

fn query_matches_service(qname: &str, qtype: u16, service: &str, instance: &str) -> bool {
    let instance_service = format!("{instance}.{service}");
    let instance_host = format!("{instance}.local");
    match qtype {
        // PTR service browse, including Xiaomi's instance-qualified probe.
        12 => qname == service || qname == instance_service,
        // SRV and TXT resolve the service instance.
        16 | 33 => qname == instance_service,
        // A and AAAA resolve the SRV target hostname.
        1 | 28 => qname == instance_host,
        // ANY can target the service, service instance, or target host.
        255 => qname == service || qname == instance_service || qname == instance_host,
        _ => false,
    }
}

fn send_discovery_reply(
    socket: &UdpSocket,
    query: &[u8],
    source: SocketAddr,
    destination: SocketAddr,
    service: &str,
    response: &[u8],
    events: &EventEmitter,
) {
    let mut reply = response.to_vec();
    reply[..2].copy_from_slice(&query[..2]);
    let sent = socket.send_to(&reply, destination);
    events(json!({
        "event": "discovery_query",
        "protocol": "xiaomi_miplay",
        "service": service,
        "source": source.to_string(),
        "destination": destination.to_string(),
        "bytes": reply.len(),
        "query_wire_hex": hex::encode(query),
        "reply_wire_hex": hex::encode(&reply),
        "unicast": destination == source,
        "success": sent.is_ok(),
    }));
}

#[cfg(test)]
fn is_lyra_query(packet: &[u8]) -> bool {
    is_service_query(packet, LYRA_SERVICE_QNAME)
}

#[cfg(test)]
fn is_service_query(packet: &[u8], service_qname: &[u8]) -> bool {
    if packet.len() < 12 {
        return false;
    }
    if packet[2] & 0x80 != 0 {
        return false;
    }
    let target = if service_qname == LYRA_SERVICE_QNAME {
        LYRA_SERVICE_NAME
    } else {
        MI_CONNECT_SERVICE_NAME
    };
    parse_dns_questions(packet).iter().any(|(qname, qtype, _)| {
        *qtype == 12 && (qname == target || qname.ends_with(&format!(".{target}")))
    })
}

fn parse_dns_questions(packet: &[u8]) -> Vec<(String, u16, u16)> {
    if packet.len() < 12 {
        return Vec::new();
    }
    let question_count = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let mut offset = 12usize;
    let mut queries = Vec::with_capacity(question_count);
    for _ in 0..question_count {
        let Some(name) = parse_dns_name(packet, &mut offset) else {
            break;
        };
        if offset + 4 > packet.len() {
            break;
        }
        let qtype = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let qclass = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
        offset += 4;
        queries.push((name, qtype, qclass));
    }
    queries
}

fn parse_dns_name(packet: &[u8], offset: &mut usize) -> Option<String> {
    let mut cursor = *offset;
    let mut labels = Vec::new();
    let mut read_steps = 0u8;
    let mut jumped = false;

    while read_steps < 16 {
        if cursor >= packet.len() {
            return None;
        }
        let len = packet[cursor];
        if len == 0 {
            cursor += 1;
            if !jumped {
                *offset = cursor;
            }
            return Some(labels.join("."));
        }
        if len & 0xC0 == 0xC0 {
            if cursor + 1 >= packet.len() {
                return None;
            }
            let pointer = (usize::from(len & 0x3F) << 8) | usize::from(packet[cursor + 1]);
            if pointer >= packet.len() || pointer == *offset {
                return None;
            }
            if !jumped {
                *offset = cursor + 2;
                jumped = true;
            }
            cursor = pointer;
            read_steps = read_steps.saturating_add(1);
            continue;
        }
        let label_len = usize::from(len);
        let start = cursor + 1;
        let end = start + label_len;
        if end > packet.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&packet[start..end]).into_owned());
        cursor = end;
        read_steps = read_steps.saturating_add(1);
        if !jumped {
            *offset = cursor;
        }
    }
    None
}

fn build_lyra_response(
    receiver_name: &str,
    instance: &str,
    media_device_id: &str,
    device_type: MiPlayDeviceType,
    local_ip: Ipv4Addr,
    link_local_v6: Option<Ipv6Addr>,
    interface_name: &str,
) -> Vec<u8> {
    let txt = build_txt_data(
        receiver_name,
        instance,
        media_device_id,
        device_type,
        local_ip,
        link_local_v6,
        interface_name,
    );
    let additional_records = if link_local_v6.is_some() { 4u16 } else { 3u16 };
    let mut packet = Vec::with_capacity(384);

    // MiPCAudio/MAFSvr uses an authoritative response with one PTR answer and
    // SRV, TXT, A and (when available) AAAA records in the additional section.
    push_u16(&mut packet, 0);
    push_u16(&mut packet, 0x8400);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, 1);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, additional_records);

    packet.extend_from_slice(LYRA_SERVICE_QNAME);
    push_u16(&mut packet, 12);
    push_u16(&mut packet, 1);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(&mut packet, 11);
    push_label(&mut packet, instance);
    push_pointer(&mut packet, 0x000c);

    // The instance name starts at byte 0x2d in the fixed PTR layout above.
    push_pointer(&mut packet, 0x002d);
    push_u16(&mut packet, 33);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(&mut packet, 17);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, LYRA_CONTROL_PORT);
    push_label(&mut packet, instance);
    // "local" starts at byte 0x1c in the service QNAME.
    push_pointer(&mut packet, 0x001c);

    push_pointer(&mut packet, 0x002d);
    push_u16(&mut packet, 16);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(&mut packet, u16::try_from(txt.len()).unwrap_or(u16::MAX));
    packet.extend_from_slice(&txt);

    // The SRV target hostname starts at byte 0x4a.
    push_pointer(&mut packet, 0x004a);
    push_u16(&mut packet, 1);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(&mut packet, 4);
    packet.extend_from_slice(&local_ip.octets());

    if let Some(ipv6) = link_local_v6 {
        push_pointer(&mut packet, 0x004a);
        push_u16(&mut packet, 28);
        push_u16(&mut packet, 0x8001);
        push_u32(&mut packet, LYRA_TTL_SECONDS);
        push_u16(&mut packet, 16);
        packet.extend_from_slice(&ipv6.octets());
    }

    packet
}

/// Re-key the current Lyra response to a same-width instance name and turn it
/// into an mDNS goodbye.  The browse cache is keyed by the PTR RDATA, so this
/// removes the historical route even if its captured TXT timestamp differs.
fn build_retired_dns_sd_goodbye(
    current_response: &[u8],
    current_instance: &str,
    retired_instance: &str,
) -> Option<Vec<u8>> {
    let current = current_instance.as_bytes();
    let retired = retired_instance.as_bytes();
    if current.is_empty() || current.len() != retired.len() {
        return None;
    }

    let mut rewritten = current_response.to_vec();
    let mut replaced = false;
    for offset in 0..=rewritten.len().saturating_sub(current.len()) {
        if rewritten[offset..].starts_with(current) {
            rewritten[offset..offset + current.len()].copy_from_slice(retired);
            replaced = true;
        }
    }
    replaced.then(|| zero_dns_record_ttls(&rewritten)).flatten()
}

/// Turn an authoritative DNS-SD response into an RFC 6762 goodbye while
/// retaining every owner name and RDATA byte exactly.  Cache eviction is
/// keyed by that complete record identity, so rebuilding only the PTR record
/// is insufficient for clients that retained the SRV/TXT address records.
fn set_dns_record_ttls(packet: &[u8], ttl_seconds: u32) -> Option<Vec<u8>> {
    if packet.len() < 12 {
        return None;
    }
    let question_count = usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    let record_count = usize::from(u16::from_be_bytes([packet[6], packet[7]]))
        + usize::from(u16::from_be_bytes([packet[8], packet[9]]))
        + usize::from(u16::from_be_bytes([packet[10], packet[11]]));
    let mut rewritten = packet.to_vec();
    let mut offset = 12usize;

    for _ in 0..question_count {
        parse_dns_name(packet, &mut offset)?;
        offset = offset.checked_add(4)?;
        if offset > packet.len() {
            return None;
        }
    }

    for _ in 0..record_count {
        parse_dns_name(packet, &mut offset)?;
        if offset.checked_add(10)? > packet.len() {
            return None;
        }
        let ttl_offset = offset + 4;
        rewritten[ttl_offset..ttl_offset + 4].copy_from_slice(&ttl_seconds.to_be_bytes());
        let data_length = usize::from(u16::from_be_bytes([packet[offset + 8], packet[offset + 9]]));
        offset = offset.checked_add(10 + data_length)?;
        if offset > packet.len() {
            return None;
        }
    }
    Some(rewritten)
}

fn zero_dns_record_ttls(packet: &[u8]) -> Option<Vec<u8>> {
    set_dns_record_ttls(packet, 0)
}

fn build_mi_connect_response(
    receiver_name: &str,
    instance: &str,
    idm_device_id: &str,
    media_device_id: &str,
    device_type: MiPlayDeviceType,
    local_ip: Ipv4Addr,
    link_local_v6: Option<Ipv6Addr>,
) -> Vec<u8> {
    let txt = build_mi_connect_txt(receiver_name, idm_device_id, media_device_id, device_type);
    let additional_records = if link_local_v6.is_some() { 4u16 } else { 3u16 };
    let mut packet = Vec::with_capacity(320);

    push_u16(&mut packet, 0);
    push_u16(&mut packet, 0x8400);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, 1);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, additional_records);

    let service_offset = u16::try_from(packet.len()).expect("mDNS packet offset");
    packet.extend_from_slice(MI_CONNECT_SERVICE_QNAME);
    let local_offset = service_offset + 1 + 11 + 1 + 4;
    push_u16(&mut packet, 12);
    push_u16(&mut packet, 1);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(
        &mut packet,
        u16::try_from(1 + instance.len() + 2).expect("MiConnect instance length"),
    );
    let instance_offset = u16::try_from(packet.len()).expect("mDNS packet offset");
    push_label(&mut packet, instance);
    push_pointer(&mut packet, service_offset);

    push_pointer(&mut packet, instance_offset);
    push_u16(&mut packet, 33);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(
        &mut packet,
        u16::try_from(6 + 1 + instance.len() + 2).expect("MiConnect SRV length"),
    );
    push_u16(&mut packet, 0);
    push_u16(&mut packet, 0);
    push_u16(&mut packet, MI_CONNECT_DISCOVERY_PORT);
    let target_offset = u16::try_from(packet.len()).expect("mDNS packet offset");
    push_label(&mut packet, instance);
    push_pointer(&mut packet, local_offset);

    push_pointer(&mut packet, instance_offset);
    push_u16(&mut packet, 16);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(
        &mut packet,
        u16::try_from(txt.len()).expect("MiConnect TXT length"),
    );
    packet.extend_from_slice(&txt);

    push_pointer(&mut packet, target_offset);
    push_u16(&mut packet, 1);
    push_u16(&mut packet, 0x8001);
    push_u32(&mut packet, LYRA_TTL_SECONDS);
    push_u16(&mut packet, 4);
    packet.extend_from_slice(&local_ip.octets());

    if let Some(ipv6) = link_local_v6 {
        push_pointer(&mut packet, target_offset);
        push_u16(&mut packet, 28);
        push_u16(&mut packet, 0x8001);
        push_u32(&mut packet, LYRA_TTL_SECONDS);
        push_u16(&mut packet, 16);
        packet.extend_from_slice(&ipv6.octets());
    }

    packet
}

fn build_mi_connect_txt(
    receiver_name: &str,
    idm_device_id: &str,
    media_device_id: &str,
    device_type: MiPlayDeviceType,
) -> Vec<u8> {
    let identity = idm_identity(idm_device_id);
    let advertised_mac = parse_hardware_address(media_device_id)
        .map(|address| base64::engine::general_purpose::STANDARD.encode(address));
    let advertised_short_id = std::env::var("FUSIONPLAY_MIPLAY_DIAGNOSTIC_ACCOUNT_ID")
        .ok()
        .filter(|value| value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .unwrap_or_else(|| identity.short_id.clone());
    // `idHash` is Base64 of the three-character IDM id, not Base64 of the
    // first three raw digest bytes.  For example Xiaomi's logged-in `bsl`
    // identity is advertised as `YnNs`.
    let id_hash = base64::engine::general_purpose::STANDARD.encode(advertised_short_id.as_bytes());
    let mut txt = Vec::new();
    let mut values = vec![
        "version=196608".to_owned(),
        "apps=[5]".to_owned(),
        // `flags` describes the advertised MiPlay application's transport
        // capabilities, not the endpoint's device category.  The captured
        // MiPCAudio record uses 0x0a,0x01 (Base64 `CgE=`) together with
        // `apps=[5]` and the complete MiPlay appsData descriptor below.
        // Advertising only 0x08,0x01 (`CAE=`) while retaining that descriptor
        // creates an internally inconsistent record: a warm HyperOS cache can
        // mask it, but a cold-cache scan rejects the endpoint.  Keep the
        // official application capability bits. FusionPlay intentionally uses
        // the account-free legacy television route below rather than the
        // account-bound PC identity emitted by MiPCAudio.
        "flags=CgE=".to_owned(),
        format!("name={receiver_name}"),
        format!("idHash={id_hash}"),
        // This is the route-picker presentation category. The authenticated
        // session reports the same selected category so discovery and command
        // identity remain consistent.
        format!("dev={}", device_type.protocol_value()),
        "sec=2".to_owned(),
        // Preserve the descriptor emitted by Android 1.1.7. Its Lyra support
        // marker is intentionally absent: Mi Connect owns the sole picker
        // route while Lyra remains the discovery/control handshake.
        "appsData=gQAEBIMiww==".to_owned(),
    ];
    // Xiaomi's BonjourGovernor first reads `mac` from the TXT record. If the
    // field is absent it pings the IPv4 address and asks the Wi-Fi stack for a
    // neighbour-table entry. That fallback cannot resolve a Windows receiver
    // behind the phone's wireless-to-wired bridge and the endpoint is dropped
    // with `failed to get MAC address`. Mi Connect accepts Base64 of the six
    // raw hardware-address bytes here. Official PCs can omit it because the
    // vendor network service supplies the address out-of-band; a standalone
    // cross-platform receiver must advertise it explicitly.
    if let Some(mac) = advertised_mac {
        values.push(format!("mac={mac}"));
    }
    for value in values {
        let bytes = value.as_bytes();
        let length = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
        txt.push(length);
        txt.extend_from_slice(&bytes[..usize::from(length)]);
    }
    txt
}

fn parse_hardware_address(device_id: &str) -> Option<[u8; 6]> {
    let compact = device_id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>();
    if compact.len() != 12 {
        return None;
    }
    let mut address = [0u8; 6];
    for (index, byte) in address.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&compact[offset..offset + 2], 16).ok()?;
    }
    Some(address)
}

fn build_txt_data(
    receiver_name: &str,
    instance: &str,
    media_device_id: &str,
    device_type: MiPlayDeviceType,
    local_ip: Ipv4Addr,
    link_local_v6: Option<Ipv6Addr>,
    interface_name: &str,
) -> Vec<u8> {
    let app_data = base64::engine::general_purpose::STANDARD.encode(build_app_data(
        receiver_name,
        instance,
        media_device_id,
        device_type,
    ));
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    // Xiaomi does not put the middle address components into DebugInfo as
    // plain text. MiPCAudio applies a nibble-to-punctuation substitution to
    // every component except the first and last one while the authoritative
    // A/AAAA records remain unchanged. HyperOS uses this representation when
    // validating a Lyra browse result; sending otherwise valid plain-text
    // addresses makes recent phones keep browsing without starting the Lyra command channel.
    let debug_ipv4 = encode_xiaomi_debug_ip(&local_ip.to_string(), '.');
    let debug_info = match link_local_v6 {
        Some(ipv6) => {
            let debug_ipv6 = encode_xiaomi_debug_ip(&ipv6.to_string(), ':');
            format!("{{msg:reply, ifname:{interface_name}, v4:{debug_ipv4}, v6:{debug_ipv6}}}")
        }
        None => {
            format!("{{msg:reply, ifname:{interface_name}, v4:{debug_ipv4}}}")
        }
    };
    let mut txt = Vec::new();
    for value in [
        format!("AppData={app_data}"),
        "MediumType=8192".to_owned(),
        "CH=0".to_owned(),
        format!("DebugInfo={debug_info}"),
        format!("TS={timestamp}"),
    ] {
        let bytes = value.as_bytes();
        let length = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
        txt.push(length);
        txt.extend_from_slice(&bytes[..usize::from(length)]);
    }
    txt
}

fn encode_xiaomi_debug_ip(address: &str, separator: char) -> String {
    let components = address.split(separator).collect::<Vec<_>>();
    let final_index = components.len().saturating_sub(1);
    components
        .into_iter()
        .enumerate()
        .map(|(index, component)| {
            if index == 0 || index == final_index || component.is_empty() {
                component.to_owned()
            } else {
                component
                    .chars()
                    .map(|character| match character {
                        '0'..='9' => {
                            char::from_u32(u32::from('#') + character.to_digit(10).unwrap_or(0))
                                .unwrap_or(character)
                        }
                        'a'..='f' | 'A'..='F' => {
                            char::from_u32(u32::from('1') + character.to_digit(16).unwrap_or(0))
                                .unwrap_or(character)
                        }
                        _ => character,
                    })
                    .collect()
            }
        })
        .collect::<Vec<_>>()
        .join(&separator.to_string())
}

fn find_link_local_ipv6(interface_name: &str, local_ip: Ipv4Addr) -> Option<Ipv6Addr> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    let resolved_interface = interfaces
        .iter()
        .find(|interface| interface.ip() == IpAddr::V4(local_ip));
    let resolved_index = resolved_interface.and_then(|interface| interface.index);
    let resolved_name = resolved_interface
        .map(|interface| interface.name.clone())
        .unwrap_or_else(|| interface_name.to_owned());
    let portable_result = interfaces.into_iter().find_map(|interface| {
        let IpAddr::V6(address) = interface.ip() else {
            return None;
        };
        // Windows can expose the IPv4 and IPv6 rows of one adapter with
        // different or missing indices through GetAdaptersAddresses.  The
        // previous exclusive index comparison discarded the valid link-local
        // IPv6 row even when both rows had the same adapter name, producing a
        // three-additional-record response that diverged from MiPCAudio's
        // captured PTR/SRV/TXT/A/AAAA layout.
        let same_index = matches!(
            (resolved_index, interface.index),
            (Some(expected), Some(actual)) if expected == actual
        );
        let same_name = interface.name.eq_ignore_ascii_case(&resolved_name);
        let same_adapter = same_index || same_name;
        (same_adapter && address.is_unicast_link_local()).then_some(address)
    });
    if portable_result.is_some() {
        return portable_result;
    }

    // `if-addrs` does not expose the IPv6 link-local row on some Windows
    // systems even though the address is visible in ipconfig. Xiaomi's
    // MiPCAudio includes that row as an AAAA record in every Lyra response.
    // Query the Windows IP Helper table directly as a self-contained fallback
    // so FusionPlay produces the same four-record reply on wired adapters.
    #[cfg(windows)]
    {
        find_windows_link_local_ipv6(resolved_index)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn find_mi_connect_ipv6(
    interface_name: &str,
    local_ip: Ipv4Addr,
    link_local_fallback: Option<Ipv6Addr>,
) -> Option<Ipv6Addr> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    let selected = interfaces
        .iter()
        .find(|interface| interface.ip() == IpAddr::V4(local_ip));
    let selected_index = selected.and_then(|interface| interface.index);
    let selected_name = selected
        .map(|interface| interface.name.clone())
        .unwrap_or_else(|| interface_name.to_owned());

    // MiPCAudio uses the adapter's globally routable IPv6 address in the
    // `_mi-connect` AAAA record, while its Lyra record deliberately uses the
    // link-local address. Keep those two service records independent instead
    // of reusing Lyra's fe80:: address for IDM discovery.
    interfaces
        .into_iter()
        .find_map(|interface| {
            let IpAddr::V6(address) = interface.ip() else {
                return None;
            };
            let same_index = matches!(
                (selected_index, interface.index),
                (Some(expected), Some(actual)) if expected == actual
            );
            let same_name = interface.name.eq_ignore_ascii_case(&selected_name);
            let usable_global = !address.is_unicast_link_local()
                && !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast();
            ((same_index || same_name) && usable_global).then_some(address)
        })
        .or(link_local_fallback)
}

#[cfg(windows)]
fn find_windows_link_local_ipv6(interface_index: Option<u32>) -> Option<Ipv6Addr> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetUnicastIpAddressTable, MIB_UNICASTIPADDRESS_TABLE,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET6;

    let expected_index = interface_index?;
    let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
    // SAFETY: Windows allocates `table` on success and documents that it must
    // be released by FreeMibTable. The pointer is checked before dereference.
    let status = unsafe { GetUnicastIpAddressTable(AF_INET6, &mut table) };
    if status != 0 || table.is_null() {
        return None;
    }

    // SAFETY: MIB_UNICASTIPADDRESS_TABLE is a variable-length table whose
    // first row is `Table[0]`; NumEntries gives the contiguous row count.
    let result = unsafe {
        let count = usize::try_from((*table).NumEntries).unwrap_or(0);
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        rows.iter().find_map(|row| {
            if row.InterfaceIndex != expected_index || row.Address.si_family != AF_INET6 {
                return None;
            }
            let octets = row.Address.Ipv6.sin6_addr.u.Byte;
            let address = Ipv6Addr::from(octets);
            address.is_unicast_link_local().then_some(address)
        })
    };

    // SAFETY: `table` was allocated by GetUnicastIpAddressTable above and has
    // not been freed or aliased for mutation.
    unsafe { FreeMibTable(table.cast()) };
    result
}

fn lyra_instance(device_id: &str, local_ip: Ipv4Addr) -> String {
    let hex: String = device_id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    if hex.len() >= 8 {
        return hex[hex.len() - 8..].to_ascii_uppercase();
    }
    let octets = local_ip.octets();
    format!(
        "{:02X}{:02X}{:02X}{:02X}",
        octets[0], octets[1], octets[2], octets[3]
    )
}

fn legacy_mi_connect_instance(receiver_name: &str, device_id: &str) -> String {
    let suffix = idm_identity(device_id).instance_suffix;
    let mut name = receiver_name.to_owned();
    let maximum_name_bytes = 63usize.saturating_sub(suffix.len() + 2);
    while name.len() > maximum_name_bytes {
        name.pop();
    }
    if name.is_empty() {
        name.push_str("FusionPlay");
    }
    format!("{name}({suffix})")
}

fn mi_connect_instance_for_platform(
    _is_android: bool,
    receiver_name: &str,
    device_id: &str,
    _lyra_instance_name: &str,
) -> String {
    legacy_mi_connect_instance(receiver_name, device_id)
}

fn build_app_data(
    receiver_name: &str,
    instance: &str,
    _hardware_address: &str,
    device_type: MiPlayDeviceType,
) -> Vec<u8> {
    let mut instance_bytes = hex::decode(instance).unwrap_or_else(|_| vec![0, 0, 0, 1]);
    if instance_bytes.len() > 4 {
        instance_bytes = instance_bytes[instance_bytes.len() - 4..].to_vec();
    }
    while instance_bytes.len() < 4 {
        instance_bytes.insert(0, 0);
    }

    let mut name = receiver_name.as_bytes().to_vec();
    while name.len() > 63 {
        name.pop();
        while std::str::from_utf8(&name).is_err() {
            name.pop();
        }
    }

    let mut payload = vec![0x00, 0x40, lyra_discovery_device_type(device_type)];
    payload.extend_from_slice(&instance_bytes);
    payload.extend_from_slice(&[0x00, 0x05, 0x19, 0x24]);
    payload.extend_from_slice(&[
        0x10, 0x01, 0x03, 0x0a, 0x03, 0x01, 0xda, 0xae, 0x01, 0x01, 0x80, 0x02,
    ]);
    payload.push(name.len() as u8);
    payload.extend_from_slice(&name);

    payload.extend_from_slice(&[0x25, 0x01, 0x03]);
    payload
}

fn push_label(packet: &mut Vec<u8>, label: &str) {
    let bytes = label.as_bytes();
    let length = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
    packet.push(length);
    packet.extend_from_slice(&bytes[..usize::from(length)]);
}

fn push_pointer(packet: &mut Vec<u8>, offset: u16) {
    push_u16(packet, 0xc000 | offset);
}

fn push_u16(packet: &mut Vec<u8>, value: u16) {
    packet.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(packet: &mut Vec<u8>, value: u32) {
    packet.extend_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::{
        MI_CONNECT_SERVICE_QNAME, build_app_data, build_lyra_response, build_mi_connect_response,
        build_mi_connect_txt, build_retired_dns_sd_goodbye, discovery_goodbye_repeat_interval,
        discovery_platform_policy, encode_xiaomi_debug_ip, idm_identity, is_lyra_query,
        is_service_query, legacy_mi_connect_instance, lyra_discovery_device_type, lyra_instance,
        mi_connect_instance_for_platform, parse_dns_name, persistent_mi_connect_discovery,
        query_matches_service, set_dns_record_ttls, zero_dns_record_ttls,
    };
    use crate::MiPlayDeviceType;
    use base64::Engine;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    #[test]
    fn android_uses_the_same_on_demand_pair_as_desktop() {
        let android = discovery_platform_policy(true);
        assert!(android.publish_app_lyra);
        assert!(!persistent_mi_connect_discovery(true, false));
        assert!(persistent_mi_connect_discovery(true, true));

        let desktop = discovery_platform_policy(false);
        assert!(desktop.publish_app_lyra);
        assert!(!persistent_mi_connect_discovery(false, false));
        assert!(persistent_mi_connect_discovery(false, true));
    }

    #[test]
    fn listener_restart_goodbyes_do_not_delay_the_replacement_socket() {
        assert_eq!(
            discovery_goodbye_repeat_interval("shutdown"),
            Duration::ZERO
        );
        assert_eq!(
            discovery_goodbye_repeat_interval("persistent_route_retirement"),
            Duration::ZERO
        );
        assert_eq!(
            discovery_goodbye_repeat_interval("on_demand_scan_expired"),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn app_data_contains_dynamic_utf8_name_and_length() {
        let payload = build_app_data(
            "FusionPlay",
            "2433CD31",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
        );
        let name_offset = payload
            .windows("FusionPlay".len())
            .position(|window| window == b"FusionPlay")
            .unwrap();
        assert_eq!(payload[name_offset - 1], "FusionPlay".len() as u8);
        assert_eq!(&payload[3..7], &[0x24, 0x33, 0xcd, 0x31]);

        let wired_payload = build_app_data(
            "Asus",
            "BC250543",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
        );
        assert!(wired_payload.windows(4).any(|window| window == b"Asus"));
        assert!(!wired_payload.windows(4).any(|window| window == b"wlan"));
    }

    #[test]
    fn television_discovery_uses_the_official_lyra_tv_category() {
        let payload = build_app_data(
            "FusionPlay",
            "2433CD31",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
        );
        assert_eq!(payload[2], 3);
        let txt = build_mi_connect_txt("FusionPlay", "idm-test", "", MiPlayDeviceType::Television);
        let expected = "dev=2";
        assert!(
            txt.windows(expected.len())
                .any(|window| window == expected.as_bytes())
        );
        // This is the private MiPlay terminal capability marker, not the
        // public device category exposed by `_mi-connect` and command 31.
        assert!(payload.ends_with(&[0x25, 0x01, 0x03]));
    }

    #[test]
    fn every_identity_uses_its_official_lyra_discovery_projection() {
        for (device_type, expected_lyra_type) in [
            (MiPlayDeviceType::Vehicle, 8),
            (MiPlayDeviceType::Television, 3),
            (MiPlayDeviceType::Tablet, 2),
            (MiPlayDeviceType::Speaker, 5),
            (MiPlayDeviceType::DisplaySpeaker, 5),
        ] {
            let payload =
                build_app_data("FusionPlay", "2433CD31", "A0-36-BC-25-05-43", device_type);
            assert_eq!(
                payload[2], expected_lyra_type,
                "unexpected Lyra projection for {device_type:?}",
            );
            assert_eq!(lyra_discovery_device_type(device_type), expected_lyra_type,);
            assert_ne!(payload[2], 0x15, "must not advertise generic PC");
        }
    }

    #[test]
    fn mi_connect_advertises_every_user_selectable_device_type() {
        for (device_type, expected) in [
            (MiPlayDeviceType::Vehicle, "dev=5"),
            (MiPlayDeviceType::Television, "dev=2"),
            (MiPlayDeviceType::Tablet, "dev=18"),
            (MiPlayDeviceType::Speaker, "dev=4"),
            (MiPlayDeviceType::DisplaySpeaker, "dev=16"),
        ] {
            let txt = build_mi_connect_txt("FusionPlay", "idm-test", "", device_type);
            assert!(
                txt.windows(expected.len())
                    .any(|window| window == expected.as_bytes()),
                "missing {expected}",
            );
        }
    }

    #[test]
    fn debug_ip_encoding_matches_mipcaudio_wire_format() {
        assert_eq!(
            encode_xiaomi_debug_ip("192.168.31.128", '.'),
            "192.$)+.&$.128"
        );
        assert_eq!(
            encode_xiaomi_debug_ip("fe80::4c8b:a77c:e55a:46d", ':'),
            "fe80::'=+<:;**=:?((;:46d"
        );
        assert_eq!(
            encode_xiaomi_debug_ip("2401:7e00:c40:8160:596e:b1b4:d192:8cd4", ':'),
            "2401:*?##:='#:+$)#:(,)?:<$<':>$,%:8cd4"
        );
    }

    #[test]
    fn instance_falls_back_to_ipv4() {
        assert_eq!(
            lyra_instance("invalid", Ipv4Addr::new(192, 168, 31, 128)),
            "C0A81F80"
        );
    }

    #[test]
    fn anonymous_idm_identity_matches_xiaomi_runtime_algorithm() {
        let identity = idm_identity("c86c85e8-cd15-4ebd-b898-c15e75deb923");
        assert_eq!(
            identity.did_hash,
            "sly3iRBCZ7Vh8cCTmApQFoguXhDu6UoXG_3n2Rht83w"
        );
        assert_eq!(identity.short_id, "sly");
        assert_eq!(identity.instance_suffix, "sly3iRBCZ7");
        assert_eq!(
            legacy_mi_connect_instance("ASUS", "c86c85e8-cd15-4ebd-b898-c15e75deb923"),
            "ASUS(sly3iRBCZ7)"
        );

        let txt = build_mi_connect_txt(
            "ASUS",
            "c86c85e8-cd15-4ebd-b898-c15e75deb923",
            "",
            MiPlayDeviceType::Television,
        );
        assert!(
            txt.windows(b"idHash=c2x5".len())
                .any(|window| window == b"idHash=c2x5")
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode("c2x5")
                .unwrap(),
            b"sly"
        );
    }

    #[test]
    fn xiaomi_legacy_unicast_query_is_recognized() {
        let query = hex::decode(
            "0000000000010000000000000a5f6c7972612d6d646e73045f756470056c6f63616c00000c8001",
        )
        .unwrap();
        assert!(is_lyra_query(&query));
        let mut response = query.clone();
        response[2] = 0x84;
        assert!(!is_lyra_query(&response));
    }

    #[test]
    fn service_instance_and_host_queries_are_recognized() {
        assert!(query_matches_service(
            "_lyra-mdns._udp.local",
            12,
            "_lyra-mdns._udp.local",
            "2433CD31",
        ));
        assert!(query_matches_service(
            "2433CD31._lyra-mdns._udp.local",
            33,
            "_lyra-mdns._udp.local",
            "2433CD31",
        ));
        assert!(query_matches_service(
            "2433CD31.local",
            28,
            "_lyra-mdns._udp.local",
            "2433CD31",
        ));
        assert!(query_matches_service(
            "2433CD31.local",
            255,
            "_lyra-mdns._udp.local",
            "2433CD31",
        ));
        assert!(!query_matches_service(
            "other.local",
            1,
            "_lyra-mdns._udp.local",
            "2433CD31",
        ));
    }

    #[test]
    fn response_matches_hyperos_miplay_record_layout() {
        let response = build_lyra_response(
            "洛茜不嘻嘻",
            "2433CD31",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
            Ipv4Addr::new(192, 168, 31, 128),
            Some("fe80::4c8b:a77c:e55a:46d".parse::<Ipv6Addr>().unwrap()),
            "以太网",
        );
        assert_eq!(&response[..12], &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 4]);
        // The record includes dynamic UTF-8 name and timestamp fields, so its
        // exact size is not a protocol invariant. Keep a conservative lower
        // bound and validate the required fields individually below.
        assert!(response.len() >= 300, "response length: {}", response.len());
        assert!(response.windows(2).any(|window| window == [0x14, 0xe9]));
        assert!(
            response
                .windows("MediumType=8192".len())
                .any(|window| window == b"MediumType=8192")
        );
        assert!(
            response
                .windows("AppData=AEADJDPNMQAFGSQQAQMKAwHargEBgAIP".len())
                .any(|window| window == b"AppData=AEADJDPNMQAFGSQQAQMKAwHargEBgAIP")
        );
        assert!(
            response
                .windows(b"v4:192.$)+.&$.128".len())
                .any(|window| window == b"v4:192.$)+.&$.128")
        );
        assert!(
            response
                .windows(b"v6:fe80::'=+<:;**=:?((;:46d".len())
                .any(|window| window == b"v6:fe80::'=+<:;**=:?((;:46d")
        );
    }

    #[test]
    fn mi_connect_response_matches_current_hyperos_layout() {
        let instance = legacy_mi_connect_instance("ASUS", "c86c85e8-cd15-4ebd-b898-c15e75deb923");
        let response = build_mi_connect_response(
            "ASUS",
            &instance,
            "c86c85e8-cd15-4ebd-b898-c15e75deb923",
            "",
            MiPlayDeviceType::Television,
            Ipv4Addr::new(192, 168, 31, 128),
            Some("2401:7e00:c40:8160:d896:44bd:ac07:390a".parse().unwrap()),
        );
        assert_eq!(&response[..12], &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 4]);
        assert_eq!(response.len(), 249);
        assert!(response.windows(2).any(|window| window == [0xdd, 0x5a]));
        for expected in [
            b"version=196608".as_slice(),
            b"apps=[5]".as_slice(),
            b"flags=CgE=".as_slice(),
            b"name=ASUS".as_slice(),
            b"dev=2".as_slice(),
            b"sec=2".as_slice(),
            b"appsData=gQAEBIMiww==".as_slice(),
        ] {
            assert!(
                response
                    .windows(expected.len())
                    .any(|window| window == expected)
            );
        }
        assert!(
            !response
                .windows(b"flags=CAE=".len())
                .any(|window| window == b"flags=CAE="),
            "the shortened capability mask is rejected by cold-cache scans"
        );
        assert!(!response.windows(4).any(|window| window == b"mac="));
    }

    #[test]
    fn mi_connect_wired_receiver_advertises_raw_hardware_address() {
        let txt = build_mi_connect_txt(
            "ASUS",
            "c86c85e8-cd15-4ebd-b898-c15e75deb923",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
        );
        assert!(
            txt.windows(b"mac=oDa8JQVD".len())
                .any(|window| window == b"mac=oDa8JQVD")
        );
    }

    #[test]
    fn xiaomi_mi_connect_query_is_recognized() {
        let query = hex::decode(
            "0000000000010000000000000b5f6d692d636f6e6e656374045f756470056c6f63616c00000c8001",
        )
        .unwrap();
        assert!(is_service_query(&query, MI_CONNECT_SERVICE_QNAME));
        assert!(!is_lyra_query(&query));
    }

    #[test]
    fn goodbye_preserves_records_and_zeros_every_ttl() {
        let response = build_lyra_response(
            "ASUS",
            "2433CD31",
            "A0-36-BC-25-05-43",
            MiPlayDeviceType::Television,
            Ipv4Addr::new(192, 168, 31, 128),
            Some("fe80::4c8b:a77c:e55a:46d".parse().unwrap()),
            "Ethernet",
        );
        let goodbye = zero_dns_record_ttls(&response).expect("valid DNS-SD response");
        assert_eq!(response.len(), goodbye.len());
        assert_eq!(&response[..12], &goodbye[..12]);

        let record_count = usize::from(u16::from_be_bytes([goodbye[6], goodbye[7]]))
            + usize::from(u16::from_be_bytes([goodbye[8], goodbye[9]]))
            + usize::from(u16::from_be_bytes([goodbye[10], goodbye[11]]));
        let mut offset = 12usize;
        for _ in 0..record_count {
            parse_dns_name(&goodbye, &mut offset).expect("record owner");
            assert_eq!(&goodbye[offset + 4..offset + 8], &[0, 0, 0, 0]);
            let data_length = usize::from(u16::from_be_bytes([
                goodbye[offset + 8],
                goodbye[offset + 9],
            ]));
            offset += 10 + data_length;
        }
        assert_eq!(offset, goodbye.len());
    }

    #[test]
    fn retired_lyra_goodbye_evicts_the_captured_instance() {
        let response = build_lyra_response(
            "Asus",
            "BC4FCCD1",
            "91B48B11",
            MiPlayDeviceType::Television,
            Ipv4Addr::new(192, 168, 31, 128),
            Some("fe80::4c8b:a77c:e55a:46d".parse().unwrap()),
            "Ethernet",
        );
        let goodbye = build_retired_dns_sd_goodbye(&response, "BC4FCCD1", "2433CD31")
            .expect("same-width historical identity should be rewritten");

        assert!(
            goodbye
                .windows(b"2433CD31".len())
                .any(|window| window == b"2433CD31")
        );
        assert!(
            !goodbye
                .windows(b"BC4FCCD1".len())
                .any(|window| window == b"BC4FCCD1")
        );
        assert_eq!(
            build_retired_dns_sd_goodbye(&response, "BC4FCCD1", "SHORT"),
            None
        );
    }

    #[test]
    fn android_keeps_the_1_1_7_legacy_mi_connect_owner() {
        let lyra_instance = "8EFB9219";
        assert_eq!(
            mi_connect_instance_for_platform(
                true,
                "Xiaomi17ProMax",
                "c86c85e8-cd15-4ebd-b898-c15e75deb923",
                lyra_instance,
            ),
            "Xiaomi17ProMax(sly3iRBCZ7)",
        );
        assert_eq!(
            mi_connect_instance_for_platform(
                false,
                "ASUS",
                "c86c85e8-cd15-4ebd-b898-c15e75deb923",
                lyra_instance,
            ),
            "ASUS(sly3iRBCZ7)",
        );
    }

    #[test]
    fn ttl_rewrite_preserves_records_and_sets_every_ttl() {
        let response = build_mi_connect_response(
            "ASUS",
            "2433CD31",
            "idm-test",
            "media-test",
            MiPlayDeviceType::Television,
            Ipv4Addr::new(192, 168, 31, 128),
            None,
        );
        let rewritten = set_dns_record_ttls(&response, 10).expect("valid DNS-SD response");
        assert_eq!(response.len(), rewritten.len());

        let record_count = usize::from(u16::from_be_bytes([rewritten[6], rewritten[7]]))
            + usize::from(u16::from_be_bytes([rewritten[8], rewritten[9]]))
            + usize::from(u16::from_be_bytes([rewritten[10], rewritten[11]]));
        let mut offset = 12usize;
        for _ in 0..record_count {
            parse_dns_name(&rewritten, &mut offset).expect("record owner");
            assert_eq!(&rewritten[offset + 4..offset + 8], &10u32.to_be_bytes());
            let data_length = usize::from(u16::from_be_bytes([
                rewritten[offset + 8],
                rewritten[offset + 9],
            ]));
            offset += 10 + data_length;
        }
        assert_eq!(offset, rewritten.len());
    }
}
