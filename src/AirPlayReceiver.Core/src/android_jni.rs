use crate::events::{EventCallback, EventSink};
use crate::host::{Settings, run_receiver_with_arbiter};
use crate::network_identity::{
    hardware_address_from_ipv6_eui64, normalize_hardware_address, select_hardware_address,
};
use crate::takeover::PlaybackArbiter;
use anyhow::{Context, Result};
use fusionplay_miplay_sdk::{
    MediaAction, MiPlayDeviceType, MiPlayReceiver, ReceiverConfig, ReceiverController,
    ReceiverIdentity,
};
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::{JNIEnv, JavaVM};
use serde_json::{Value, json};
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

fn java_string(env: &mut JNIEnv, value: JString) -> Result<String> {
    if value.as_raw().is_null() {
        anyhow::bail!("required JNI string was null");
    }
    Ok(env
        .get_string(&value)
        .context("unable to read JNI string")?
        .into())
}

struct AndroidHost {
    vm: JavaVM,
    callback: GlobalRef,
    runtime: Runtime,
    core_events: Arc<EventSink>,
    core_arbiter: Arc<PlaybackArbiter>,
    airplay: Mutex<CoreRuntimeSlot>,
    dlna: Mutex<CoreRuntimeSlot>,
    miplay: Mutex<Option<MiPlayReceiver>>,
    miplay_controller: Mutex<Option<ReceiverController>>,
    miplay_identity: Mutex<Option<ActiveMiPlayIdentity>>,
}

#[derive(Default)]
struct CoreRuntimeSlot {
    commands: Option<mpsc::UnboundedSender<String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AndroidCoreProtocol {
    AirPlay,
    Dlna,
}

impl AndroidCoreProtocol {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "airplay" => Some(Self::AirPlay),
            "dlna" => Some(Self::Dlna),
            _ => None,
        }
    }

    fn thread_name(self) -> &'static str {
        match self {
            Self::AirPlay => "fusionplay-airplay",
            Self::Dlna => "fusionplay-dlna",
        }
    }

    fn settings(
        self,
        name: String,
        state_path: PathBuf,
        output_device: Option<String>,
    ) -> Settings {
        Settings {
            name,
            pin: None,
            state_path,
            output_device,
            airplay_enabled: self == Self::AirPlay,
            dlna_enabled: self == Self::Dlna,
        }
    }
}

fn core_slot(host: &AndroidHost, protocol: AndroidCoreProtocol) -> &Mutex<CoreRuntimeSlot> {
    match protocol {
        AndroidCoreProtocol::AirPlay => &host.airplay,
        AndroidCoreProtocol::Dlna => &host.dlna,
    }
}

fn stop_core_protocol(host: &AndroidHost, protocol: AndroidCoreProtocol) {
    let (commands, thread) = {
        let mut runtime = core_slot(host, protocol).lock().expect("core runtime");
        (runtime.commands.take(), runtime.thread.take())
    };
    if let Some(tx) = commands {
        let _ = tx.send(r#"{"command":"shutdown"}"#.to_owned());
    }
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveMiPlayIdentity {
    receiver_name: String,
    local_ip: Ipv4Addr,
    interface_name: String,
    hardware_address: String,
    output_device: Option<String>,
    device_type: MiPlayDeviceType,
}

static HOST: OnceLock<Mutex<Option<AndroidHost>>> = OnceLock::new();

fn host_slot() -> &'static Mutex<Option<AndroidHost>> {
    HOST.get_or_init(|| Mutex::new(None))
}

fn emit_java(vm: &JavaVM, callback: &GlobalRef, method: &str, line: &str, is_error: bool) {
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let Ok(payload) = env.new_string(line) else {
        return;
    };
    let result = if method == "onNativeLog" {
        env.call_method(
            callback,
            method,
            "(Ljava/lang/String;Z)V",
            &[
                JValue::Object(payload.as_ref()),
                JValue::Bool(u8::from(is_error)),
            ],
        )
    } else {
        env.call_method(
            callback,
            method,
            "(Ljava/lang/String;)V",
            &[JValue::Object(payload.as_ref())],
        )
    };
    if let Err(error) = result {
        let _ = env.exception_clear();
        eprintln!("FusionPlay JNI callback {method} failed: {error}");
    }
}

#[derive(Clone, Copy)]
struct VmPtr(*mut jni::sys::JavaVM);
unsafe impl Send for VmPtr {}
unsafe impl Sync for VmPtr {}

fn core_callback(vm_ptr: VmPtr, callback: GlobalRef) -> EventCallback {
    Arc::new(move |line: String| {
        emit_named(vm_ptr, &callback, "onCoreEvent", &line);
    })
}

fn emit_named(vm_ptr: VmPtr, callback: &GlobalRef, method: &str, line: &str) {
    let vm = cloned_vm(vm_ptr.0);
    emit_java(&vm, callback, method, line, false);
}

fn wrap_xiaomi_event(value: Value) -> String {
    let event_name = value
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("sdk")
        .to_owned();
    json!({
        "type": "xiaomi_event",
        "event": event_name,
        "payload": value,
    })
    .to_string()
}

fn classify_interface(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("wlan") || lower.contains("wifi") || lower.starts_with("wl") {
        "physical_wifi"
    } else if lower.starts_with("eth") || lower.starts_with("en") && !lower.starts_with("en0") {
        "physical_ethernet"
    } else if lower.contains("rmnet") || lower.contains("ccmni") || lower.contains("wwan") {
        "other_virtual"
    } else if lower.contains("tun") || lower.contains("tap") || lower.contains("wg") {
        "tunnel"
    } else if lower.contains("lo") {
        "loopback"
    } else {
        "other_virtual"
    }
}

fn interface_hardware_address(interface_name: &str) -> Option<String> {
    if interface_name.is_empty()
        || interface_name == "."
        || interface_name == ".."
        || interface_name.contains(['/', '\\'])
    {
        return None;
    }
    fs::read_to_string(
        Path::new("/sys/class/net")
            .join(interface_name)
            .join("address"),
    )
    .ok()
    .and_then(|value| normalize_hardware_address(value.trim()))
}

fn interface_eui64_hardware_address(interface_name: &str, local_ip: Ipv4Addr) -> Option<String> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    let selected_interface = interfaces
        .iter()
        .find(|interface| interface.ip() == IpAddr::V4(local_ip));
    let selected_index = selected_interface.and_then(|interface| interface.index);
    let selected_name = selected_interface
        .map(|interface| interface.name.clone())
        .unwrap_or_else(|| interface_name.to_owned());
    interfaces.into_iter().find_map(|interface| {
        let IpAddr::V6(address) = interface.ip() else {
            return None;
        };
        let same_index = matches!(
            (selected_index, interface.index),
            (Some(expected), Some(actual)) if expected == actual
        );
        let same_name = interface.name.eq_ignore_ascii_case(&selected_name);
        (address.is_unicast_link_local() && (same_index || same_name))
            .then(|| hardware_address_from_ipv6_eui64(address))
            .flatten()
    })
}

fn list_network_adapters_json() -> Result<Value> {
    let interfaces = if_addrs::get_if_addrs().context("unable to enumerate network interfaces")?;
    let mut adapters = Vec::new();
    let mut auto_selected = None;
    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }
        let ipv4 = match interface.ip() {
            std::net::IpAddr::V4(address) if !address.is_loopback() && !address.is_link_local() => {
                address
            }
            _ => continue,
        };
        let classification = classify_interface(&interface.name);
        let eligible = matches!(classification, "physical_wifi" | "physical_ethernet");
        let id = format!("{}-{}", interface.name, interface.index.unwrap_or(0));
        let hardware_address = interface_hardware_address(&interface.name);
        if eligible && auto_selected.is_none() {
            auto_selected = Some(id.clone());
            if classification == "physical_ethernet" {
                // Keep Ethernet if we later see Wi-Fi first; the next ethernet
                // pass is handled by preferring ethernet below.
            }
        }
        adapters.push(json!({
            "id": id,
            "name": interface.name,
            "description": interface.name,
            "interface_type": classification,
            "interface_index": interface.index.unwrap_or(0),
            "ipv4_address": ipv4.to_string(),
            "mac_address": hardware_address,
            "is_up": true,
            "classification": classification,
            "auto_eligible": eligible,
            "manual_eligible": eligible,
            "is_default_route": false,
            "warning": if eligible {
                Value::Null
            } else {
                Value::String("小米妙播仅支持真实物理有线或 Wi-Fi".to_owned())
            },
        }));
    }
    if let Some(ethernet) = adapters.iter().find(|adapter| {
        adapter.get("classification").and_then(Value::as_str) == Some("physical_ethernet")
            && adapter.get("auto_eligible").and_then(Value::as_bool) == Some(true)
    }) {
        auto_selected = ethernet
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned);
    } else if let Some(wifi) = adapters.iter().find(|adapter| {
        adapter.get("classification").and_then(Value::as_str) == Some("physical_wifi")
            && adapter.get("auto_eligible").and_then(Value::as_bool) == Some(true)
    }) {
        auto_selected = wifi.get("id").and_then(Value::as_str).map(str::to_owned);
    }
    Ok(json!({
        "adapters": adapters,
        "auto_selected_adapter_id": auto_selected,
    }))
}

fn load_or_create_identity(directory: &Path) -> Result<ReceiverIdentity> {
    fs::create_dir_all(directory)?;
    let path = directory.join("miplay-identity-v1.json");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(value) = serde_json::from_str::<Value>(&text)
    {
        if let (Some(idm), Some(lyra), Some(media)) = (
            value.get("idm_instance_id").and_then(Value::as_str),
            value.get("lyra_instance_id").and_then(Value::as_str),
            value.get("media_device_id").and_then(Value::as_str),
        ) {
            return Ok(ReceiverIdentity::new(idm, lyra, media));
        }
    }
    let idm = uuid_v4();
    let lyra = random_hex(8);
    let media = random_hex(8);
    let identity = ReceiverIdentity::new(idm.clone(), lyra.clone(), media.clone());
    fs::write(
        path,
        serde_json::to_string_pretty(&json!({
            "idm_instance_id": idm,
            "lyra_instance_id": lyra,
            "media_device_id": media,
        }))?,
    )?;
    Ok(identity)
}

fn uuid_v4() -> String {
    let mut bytes = [0_u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
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

fn random_hex(length: usize) -> String {
    let mut bytes = vec![0_u8; length.div_ceil(2)];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    bytes
        .iter()
        .flat_map(|byte| format!("{byte:02X}").chars().collect::<Vec<_>>())
        .take(length)
        .collect()
}

fn media_action(name: &str, position_ms: i64) -> Option<MediaAction> {
    match name {
        "play" | "resume" => Some(MediaAction::Resume),
        "pause" => Some(MediaAction::Pause),
        "toggle" | "play_pause" => Some(MediaAction::Toggle),
        "next" | "next_track" => Some(MediaAction::Next),
        "previous" | "previous_track" => Some(MediaAction::Previous),
        "seek" if position_ms >= 0 => Some(MediaAction::Seek(position_ms as u64)),
        _ => None,
    }
}

fn cloned_vm(ptr: *mut jni::sys::JavaVM) -> JavaVM {
    unsafe { JavaVM::from_raw(ptr).expect("JavaVM") }
}

fn optional_java_string(env: &mut JNIEnv, value: JString) -> Option<String> {
    if value.as_raw().is_null() {
        return None;
    }
    env.get_string(&value)
        .ok()
        .map(|value| {
            let owned: String = value.into();
            owned.trim().to_owned()
        })
        .filter(|value| !value.is_empty())
}

fn throw_java(env: &mut JNIEnv, message: &str) {
    let _ = env.throw_new("java/lang/IllegalStateException", message);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
    callback: JObject,
) {
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(error) => {
            throw_java(&mut env, &format!("JavaVM unavailable: {error}"));
            return;
        }
    };
    let global_callback = match env.new_global_ref(&callback) {
        Ok(callback) => callback,
        Err(error) => {
            throw_java(&mut env, &format!("callback ref failed: {error}"));
            return;
        }
    };
    let global_context = match env.new_global_ref(&context) {
        Ok(context) => context,
        Err(error) => {
            throw_java(&mut env, &format!("context ref failed: {error}"));
            return;
        }
    };
    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer() as *mut std::ffi::c_void,
            global_context.as_raw() as *mut std::ffi::c_void,
        );
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("fusionplay-core")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            throw_java(&mut env, &format!("runtime failed: {error}"));
            return;
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "shairplay=info,airplay_receiver_core=info".parse().unwrap()),
        )
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
    let core_events = Arc::new(EventSink::with_callback(core_callback(
        VmPtr(vm.get_java_vm_pointer()),
        global_callback.clone(),
    )));
    let core_arbiter = Arc::new(PlaybackArbiter::new(Arc::clone(&core_events)));
    let host = AndroidHost {
        vm,
        callback: global_callback,
        runtime,
        core_events,
        core_arbiter,
        airplay: Mutex::new(CoreRuntimeSlot::default()),
        dlna: Mutex::new(CoreRuntimeSlot::default()),
        miplay: Mutex::new(None),
        miplay_controller: Mutex::new(None),
        miplay_identity: Mutex::new(None),
    };
    *host_slot().lock().expect("host slot") = Some(host);
    std::mem::forget(global_context);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeStartCoreProtocol(
    mut env: JNIEnv,
    _class: JClass,
    protocol: JString,
    name: JString,
    state_path: JString,
    output_device_id: JString,
) -> jstring {
    let protocol = match java_string(&mut env, protocol)
        .ok()
        .and_then(|value| AndroidCoreProtocol::parse(&value))
    {
        Some(value) => value,
        None => {
            throw_java(&mut env, "unsupported core protocol");
            return std::ptr::null_mut();
        }
    };
    let receiver_name = match java_string(&mut env, name) {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let state_path = match java_string(&mut env, state_path) {
        Ok(value) => PathBuf::from(value),
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let output_device = optional_java_string(&mut env, output_device_id);
    let mut slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_mut() else {
        throw_java(&mut env, "nativeInit was not called");
        return std::ptr::null_mut();
    };
    let runtime_slot = core_slot(host, protocol);
    if runtime_slot
        .lock()
        .expect("core runtime")
        .commands
        .is_some()
    {
        return JObject::null().into_raw();
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let events = Arc::clone(&host.core_events);
    let arbiter = Arc::clone(&host.core_arbiter);
    let settings = protocol.settings(receiver_name, state_path, output_device);
    let runtime_handle = host.runtime.handle().clone();
    let join = std::thread::Builder::new()
        .name(protocol.thread_name().into())
        .spawn(move || {
            let _ =
                runtime_handle.block_on(run_receiver_with_arbiter(settings, events, rx, arbiter));
        });
    match join {
        Ok(handle) => {
            let mut runtime = runtime_slot.lock().expect("core runtime");
            runtime.commands = Some(tx);
            runtime.thread = Some(handle);
            JObject::null().into_raw()
        }
        Err(error) => {
            throw_java(&mut env, &format!("unable to start core: {error}"));
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeStopCoreProtocol(
    mut env: JNIEnv,
    _class: JClass,
    protocol: JString,
) {
    let Some(protocol) = java_string(&mut env, protocol)
        .ok()
        .and_then(|value| AndroidCoreProtocol::parse(&value))
    else {
        return;
    };
    let slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_ref() else { return };
    stop_core_protocol(host, protocol);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeSendCoreCommand(
    mut env: JNIEnv,
    _class: JClass,
    target: JString,
    json: JString,
) -> jni::sys::jboolean {
    let target = match java_string(&mut env, target) {
        Ok(value) => value.trim().to_ascii_lowercase(),
        Err(_) => return 0,
    };
    let line = match java_string(&mut env, json) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    let slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_ref() else {
        return 0;
    };
    let sender_for = |protocol| {
        core_slot(host, protocol)
            .lock()
            .expect("core runtime")
            .commands
            .clone()
    };
    let send = |sender: Option<mpsc::UnboundedSender<String>>, payload: String| {
        sender.is_some_and(|tx| tx.send(payload).is_ok())
    };
    let sent = match target.as_str() {
        "airplay" => send(sender_for(AndroidCoreProtocol::AirPlay), line),
        "dlna" => send(sender_for(AndroidCoreProtocol::Dlna), line),
        "all" => {
            let airplay = send(sender_for(AndroidCoreProtocol::AirPlay), line.clone());
            let dlna = send(sender_for(AndroidCoreProtocol::Dlna), line);
            airplay || dlna
        }
        "auto" => {
            let airplay = sender_for(AndroidCoreProtocol::AirPlay);
            if airplay.is_some() {
                send(airplay, line)
            } else {
                send(sender_for(AndroidCoreProtocol::Dlna), line)
            }
        }
        _ => false,
    };
    if sent { 1 } else { 0 }
}

#[cfg(test)]
mod core_protocol_tests {
    use super::AndroidCoreProtocol;

    #[test]
    fn core_protocol_names_are_explicit() {
        assert_eq!(
            AndroidCoreProtocol::parse("airplay"),
            Some(AndroidCoreProtocol::AirPlay)
        );
        assert_eq!(
            AndroidCoreProtocol::parse("DLNA"),
            Some(AndroidCoreProtocol::Dlna)
        );
        assert_eq!(AndroidCoreProtocol::parse("miplay"), None);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeListNetworkAdapters(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    match list_network_adapters_json() {
        Ok(value) => env
            .new_string(value.to_string())
            .map(|value| value.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(error) => {
            let mut env = env;
            throw_java(&mut env, &error.to_string());
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeStartMiPlay(
    mut env: JNIEnv,
    _class: JClass,
    receiver_name: JString,
    ipv4: JString,
    interface_name: JString,
    hardware_address: JString,
    identity_dir: JString,
    output_device_id: JString,
    initial_volume_percent: jint,
    device_type: jint,
) -> jstring {
    let name = match java_string(&mut env, receiver_name) {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let ipv4_text = match java_string(&mut env, ipv4) {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let interface_name = match java_string(&mut env, interface_name) {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let requested_hardware_address = optional_java_string(&mut env, hardware_address);
    let identity_dir = match java_string(&mut env, identity_dir) {
        Ok(value) => PathBuf::from(value),
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let output_device = optional_java_string(&mut env, output_device_id);
    let device_type = match MiPlayDeviceType::try_from(device_type) {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, error);
            return std::ptr::null_mut();
        }
    };
    let local_ip: Ipv4Addr = match ipv4_text.parse() {
        Ok(value) => value,
        Err(error) => {
            throw_java(&mut env, &format!("invalid IPv4: {error}"));
            return std::ptr::null_mut();
        }
    };
    let identity = match load_or_create_identity(&identity_dir) {
        Ok(identity) => identity,
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let interface_hardware_address = interface_hardware_address(&interface_name);
    let interface_eui64_hardware_address =
        interface_eui64_hardware_address(&interface_name, local_ip);
    let (hardware_address, hardware_address_source) = select_hardware_address(
        interface_hardware_address.as_deref(),
        interface_eui64_hardware_address.as_deref(),
        requested_hardware_address.as_deref(),
        &identity.idm_instance_id,
    );
    let active_identity = ActiveMiPlayIdentity {
        receiver_name: name.clone(),
        local_ip,
        interface_name: interface_name.clone(),
        hardware_address: hardware_address.clone(),
        output_device: output_device.clone(),
        device_type,
    };
    let mut slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_mut() else {
        throw_java(&mut env, "nativeInit was not called");
        return std::ptr::null_mut();
    };
    let reuse_current = host.miplay.lock().expect("miplay").is_some()
        && host
            .miplay_identity
            .lock()
            .expect("miplay identity")
            .as_ref()
            == Some(&active_identity);
    if reuse_current {
        return JObject::null().into_raw();
    }
    *host.miplay_controller.lock().expect("controller") = None;
    *host.miplay.lock().expect("miplay") = None;
    *host.miplay_identity.lock().expect("miplay identity") = None;
    let vm_ptr = VmPtr(host.vm.get_java_vm_pointer());
    let callback = host.callback.clone();
    let events: fusionplay_miplay_sdk::EventEmitter = Arc::new(move |value: Value| {
        // protocol_trace is a high-frequency wire dump used by the desktop
        // diagnostic path. Android has no consumer for it, so avoid serializing
        // it, crossing JNI, parsing it in Kotlin, and discarding it there.
        if value.get("event").and_then(Value::as_str) == Some("protocol_trace") {
            return;
        }
        emit_named(
            vm_ptr,
            &callback,
            "onXiaomiEvent",
            &wrap_xiaomi_event(value),
        );
    });
    events(json!({
        "event": "android_network_identity_ready",
        "protocol": "xiaomi_miplay",
        "interface": interface_name,
        "hardware_address": hardware_address,
        "hardware_address_source": hardware_address_source,
        "matches_selected_adapter": interface_hardware_address
            .as_deref()
            .or(interface_eui64_hardware_address.as_deref())
            .is_some_and(|address| address == active_identity.hardware_address),
    }));
    let config = ReceiverConfig::new(name, identity, local_ip, interface_name)
        .with_device_type(device_type)
        .with_output_device(output_device)
        .with_hardware_address(Some(active_identity.hardware_address.clone()))
        .with_initial_volume_percent(initial_volume_percent.clamp(0, 100) as u32);
    match MiPlayReceiver::start(config, events) {
        Ok(receiver) => {
            *host.miplay_controller.lock().expect("controller") = Some(receiver.controller());
            *host.miplay.lock().expect("miplay") = Some(receiver);
            *host.miplay_identity.lock().expect("miplay identity") = Some(active_identity);
            JObject::null().into_raw()
        }
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeStopMiPlay(
    _env: JNIEnv,
    _class: JClass,
) {
    let slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_ref() else {
        return;
    };
    *host.miplay_controller.lock().expect("controller") = None;
    *host.miplay.lock().expect("miplay") = None;
    *host.miplay_identity.lock().expect("miplay identity") = None;
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeSuspendMiPlayOutput(
    _env: JNIEnv,
    _class: JClass,
) {
    let controller = host_slot()
        .lock()
        .expect("host slot")
        .as_ref()
        .and_then(|host| host.miplay_controller.lock().expect("controller").clone());
    if let Some(controller) = controller {
        controller.suspend_output();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeResumeMiPlayOutput(
    _env: JNIEnv,
    _class: JClass,
) {
    let controller = host_slot()
        .lock()
        .expect("host slot")
        .as_ref()
        .and_then(|host| host.miplay_controller.lock().expect("controller").clone());
    if let Some(controller) = controller {
        controller.resume_output();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeSetMiPlayVolume(
    _env: JNIEnv,
    _class: JClass,
    percent: jint,
) -> jboolean {
    let controller = host_slot()
        .lock()
        .expect("host slot")
        .as_ref()
        .and_then(|host| host.miplay_controller.lock().expect("controller").clone());
    controller
        .is_some_and(|controller| controller.set_volume(percent.clamp(0, 100) as u8).is_ok())
        .into()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_airplayreceiver_desktop_nativebridge_FusionPlayNative_nativeControlMiPlay(
    mut env: JNIEnv,
    _class: JClass,
    action: JString,
    position_ms: jlong,
) -> jstring {
    let action_name = match java_string(&mut env, action) {
        Ok(value) => value.trim().to_ascii_lowercase(),
        Err(error) => {
            throw_java(&mut env, &error.to_string());
            return std::ptr::null_mut();
        }
    };
    let slot = host_slot().lock().expect("host slot");
    let Some(host) = slot.as_ref() else {
        throw_java(&mut env, "nativeInit was not called");
        return std::ptr::null_mut();
    };
    let Some(controller) = host.miplay_controller.lock().expect("controller").clone() else {
        let payload = json!({
            "succeeded": false,
            "dispatched": false,
            "confirmed": false,
            "action": action_name,
            "position_ms": if position_ms >= 0 { Value::from(position_ms) } else { Value::Null },
            "connection_preserved": true,
            "method": "jni",
            "message": "当前没有可控制的小米妙播源。",
        });
        return env
            .new_string(payload.to_string())
            .map(|value| value.into_raw())
            .unwrap_or(std::ptr::null_mut());
    };
    // Do not serialize independent UI control coroutines behind the
    // global native-host lock while waiting for a MiPlay confirmation. The
    // controller owns its session state and coalesces matching rapid commands.
    drop(slot);
    let Some(media_action) = media_action(&action_name, position_ms) else {
        throw_java(&mut env, "unknown MiPlay action");
        return std::ptr::null_mut();
    };
    // Xiaomi's receiver callback dispatches pause/resume directly to the local
    // MediaSession and does not wait for a playback-state echo. Our protocol
    // state is projected as soon as the frame enters the writer queue, so an
    // optional HyperOS echo must not delay the receiver UI either.
    let outcome = controller.send_confirmed(media_action, std::time::Duration::ZERO);
    let payload = match outcome {
        Ok(result) => json!({
            "succeeded": result.confirmed || result.dispatched,
            "dispatched": result.dispatched,
            "confirmed": result.confirmed,
            "action": action_name,
            "position_ms": if position_ms >= 0 { Value::from(position_ms) } else { Value::Null },
            "connection_preserved": true,
            "method": "jni",
            "message": if result.confirmed {
                "小米妙播控制已确认"
            } else if result.dispatched {
                "小米妙播控制已发送"
            } else {
                "小米妙播控制未执行"
            },
        }),
        Err(error) => json!({
            "succeeded": false,
            "dispatched": false,
            "confirmed": false,
            "action": action_name,
            "position_ms": if position_ms >= 0 { Value::from(position_ms) } else { Value::Null },
            "connection_preserved": true,
            "method": "jni",
            "message": error.to_string(),
        }),
    };
    env.new_string(payload.to_string())
        .map(|value| value.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
