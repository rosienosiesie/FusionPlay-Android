//! Minimal Xiaomi Lyra transport used by HyperOS MiPlay discovery.
//!
//! This implementation is based on observed wire behaviour and intentionally
//! contains no Xiaomi binaries, account client, certificate service or JNI.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use hkdf::Hkdf;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use p256::{EncodedPoint, PublicKey};
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::MiPlayDeviceType;
use crate::media::EventEmitter;
use crate::protocol::CONTROL_PORT;

pub const LYRA_COMMAND_PORT: u16 = 55_982;

const KCP_PUSH: u8 = 0x51;
const KCP_ACK: u8 = 0x52;
const LOGICAL_TRUST_LOCAL_NETWORK: u64 = 0x40;
const SESSION_KEY_SALT: [u8; 32] = [
    0x5e, 0xd5, 0xa3, 0xf8, 0x36, 0xf6, 0xb5, 0x4f, 0x7b, 0x1e, 0xfa, 0xd0, 0x27, 0x14, 0xd5, 0x17,
    0x7b, 0x8a, 0x1f, 0x0f, 0x19, 0xe3, 0x69, 0xcc, 0x0b, 0xe8, 0xd9, 0x8b, 0xa6, 0x29, 0x73, 0x17,
];

pub struct LyraControlServer {
    port: u16,
}

impl LyraControlServer {
    pub const fn port(&self) -> u16 {
        self.port
    }
}

struct PendingResponse {
    packet: Vec<u8>,
    due: Instant,
    count: u8,
}

#[derive(Default)]
struct PeerSession {
    server_sn: u32,
    responses: HashMap<u32, Vec<u8>>,
    pending: HashMap<u32, PendingResponse>,
    account_hash: Vec<u8>,
    session_key: Option<[u8; 32]>,
    client_channel_id: Option<u64>,
    server_channel_id: Option<u64>,
}

struct LogicalFrame {
    outer_scalars: Vec<(u32, u64)>,
    logical_id: u64,
    encrypted: bool,
    inner_type: u64,
    inner: Vec<u8>,
    message: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum PbValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

pub fn start_lyra_server(
    local_ip: Ipv4Addr,
    lyra_device_id: String,
    receiver_name: String,
    platform: String,
    device_type: MiPlayDeviceType,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) -> Result<LyraControlServer> {
    let socket = UdpSocket::bind((local_ip, LYRA_COMMAND_PORT)).with_context(|| {
        format!("bind Xiaomi Lyra command socket {local_ip}:{LYRA_COMMAND_PORT}")
    })?;
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .context("set Xiaomi Lyra command receive timeout")?;
    let local = socket
        .local_addr()
        .context("read Xiaomi Lyra command address")?;
    thread::Builder::new()
        .name("miplay-lyra-control".to_owned())
        .spawn(move || {
            events(json!({
                "event": "lyra_command_ready",
                "protocol": "xiaomi_miplay",
                "address": local.to_string(),
                "external_service_required": false,
            }));
            run_server(
                socket,
                lyra_device_id,
                receiver_name,
                platform,
                device_type,
                shutdown,
                events,
            );
        })
        .context("spawn Xiaomi Lyra command server")?;
    Ok(LyraControlServer {
        port: LYRA_COMMAND_PORT,
    })
}

fn run_server(
    socket: UdpSocket,
    lyra_device_id: String,
    receiver_name: String,
    platform: String,
    device_type: MiPlayDeviceType,
    shutdown: Arc<AtomicBool>,
    events: EventEmitter,
) {
    let mut sessions: HashMap<SocketAddr, PeerSession> = HashMap::new();
    let mut buffer = [0_u8; 65_535];
    while !shutdown.load(Ordering::Acquire) {
        retransmit_due(&socket, &mut sessions, &events);
        let (length, peer) = match socket.recv_from(&mut buffer) {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                events(json!({
                    "event": "error",
                    "code": "miplay_lyra_receive_failed",
                    "message": error.to_string(),
                }));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let packet = &buffer[..length];
        events(json!({
            "event": "lyra_packet_received",
            "protocol": "xiaomi_miplay",
            "peer": peer.to_string(),
            "bytes": length,
        }));
        let mut offset = 0;
        while offset + 24 <= packet.len() {
            let conv = u32::from_le_bytes(packet[offset..offset + 4].try_into().unwrap());
            let cmd = packet[offset + 4];
            let wnd = u16::from_le_bytes(packet[offset + 6..offset + 8].try_into().unwrap());
            let ts = u32::from_le_bytes(packet[offset + 8..offset + 12].try_into().unwrap());
            let sn = u32::from_le_bytes(packet[offset + 12..offset + 16].try_into().unwrap());
            let una = u32::from_le_bytes(packet[offset + 16..offset + 20].try_into().unwrap());
            let payload_len =
                u32::from_le_bytes(packet[offset + 20..offset + 24].try_into().unwrap()) as usize;
            offset += 24;
            if offset + payload_len > packet.len() {
                break;
            }
            let payload = &packet[offset..offset + payload_len];
            offset += payload_len;
            let session = sessions.entry(peer).or_default();
            if cmd == KCP_ACK {
                session.pending.remove(&sn);
                session.pending.retain(|server_sn, _| *server_sn >= una);
                continue;
            }
            if cmd != KCP_PUSH {
                continue;
            }
            let ack = kcp_header(conv, KCP_ACK, wnd.max(128), ts, sn, sn + 1, 0);
            let _ = socket.send_to(&ack, peer);
            if let Some(response) = session.responses.get(&sn) {
                let _ = socket.send_to(response, peer);
                continue;
            }

            let response = match handle_payload(
                payload,
                session,
                &lyra_device_id,
                &receiver_name,
                &platform,
                device_type,
                peer,
                &events,
            ) {
                Ok(value) => value,
                Err(error) => {
                    events(json!({
                        "event": "error",
                        "code": "miplay_lyra_frame_failed",
                        "peer": peer.to_string(),
                        "message": format!("{error:#}"),
                        "wire_hex": hex::encode(payload),
                    }));
                    None
                }
            };
            let Some(response) = response else {
                continue;
            };
            let server_sn = session.server_sn;
            session.server_sn = server_sn.wrapping_add(1);
            let mut response_packet = kcp_header(
                conv,
                KCP_PUSH,
                wnd.max(128),
                monotonic_timestamp(),
                server_sn,
                sn + 1,
                response.len() as u32,
            );
            response_packet.extend_from_slice(&response);
            let _ = socket.send_to(&response_packet, peer);
            session.responses.insert(sn, response_packet.clone());
            session.pending.insert(
                server_sn,
                PendingResponse {
                    packet: response_packet,
                    due: Instant::now() + Duration::from_millis(250),
                    count: 1,
                },
            );
        }
    }
}

fn retransmit_due(
    socket: &UdpSocket,
    sessions: &mut HashMap<SocketAddr, PeerSession>,
    events: &EventEmitter,
) {
    let now = Instant::now();
    for (peer, session) in sessions.iter_mut() {
        for (server_sn, pending) in session.pending.iter_mut() {
            if now < pending.due || pending.count >= 8 {
                continue;
            }
            let _ = socket.send_to(&pending.packet, peer);
            pending.count += 1;
            pending.due = now + Duration::from_millis(250);
            events(json!({
                "event": "lyra_response_retransmit",
                "protocol": "xiaomi_miplay",
                "peer": peer.to_string(),
                "sequence": server_sn,
                "count": pending.count,
            }));
        }
        session.pending.retain(|_, pending| pending.count < 8);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_payload(
    frame: &[u8],
    session: &mut PeerSession,
    lyra_device_id: &str,
    receiver_name: &str,
    platform: &str,
    device_type: MiPlayDeviceType,
    peer: SocketAddr,
    events: &EventEmitter,
) -> Result<Option<Vec<u8>>> {
    let (kind, body) = transport_payload(frame)?;
    events(json!({
        "event": "lyra_transport_frame_trace",
        "protocol": "xiaomi_miplay",
        "peer": peer.to_string(),
        "transport_kind": kind,
        "body_hex": hex::encode(body),
    }));
    match kind {
        0x09 => {
            let (frame_type, message) = physical_frame(body)?;
            match frame_type {
                1 => {
                    session.account_hash = extract_phone_account(body).unwrap_or_default();
                    events(json!({
                        "event": "lyra_physical_sync",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "receiver_name": receiver_name,
                        "lyra_device_type": device_type.lyra_protocol_value(),
                        "lyra_device_category": device_type.category_name(),
                        "account_hash_hex": hex::encode_upper(&session.account_hash),
                    }));
                    Ok(Some(build_device_sync_response(
                        lyra_device_id,
                        receiver_name,
                        platform,
                        device_type,
                        &session.account_hash,
                    )))
                }
                4 => Ok(Some(build_keep_alive_response(message.unwrap_or_default()))),
                6 => {
                    events(json!({
                        "event": "lyra_transport_disconnected",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                    }));
                    Ok(None)
                }
                _ => Ok(None),
            }
        }
        0x11 => {
            let logical = parse_logical_frame(body, session.session_key.as_ref())?;
            events(json!({
                "event": "lyra_logical_frame_trace",
                "protocol": "xiaomi_miplay",
                "peer": peer.to_string(),
                "logical_id": format!("{:08X}", logical.logical_id),
                "frame_type": logical.inner_type,
                "encrypted": logical.encrypted,
                "outer_scalars": &logical.outer_scalars,
                "inner_hex": hex::encode(&logical.inner),
                "message_hex": logical.message.as_deref().map(hex::encode),
                "message_utf8": logical
                    .message
                    .as_deref()
                    .and_then(|value| std::str::from_utf8(value).ok()),
            }));
            match logical.inner_type {
                5 => {
                    events(json!({
                        "event": "lyra_logical_sync",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "logical_id": format!("{:08X}", logical.logical_id),
                    }));
                    Ok(Some(build_logical_sync_response(&logical)?))
                }
                6 => {
                    let auth = logical
                        .message
                        .as_deref()
                        .context("missing Lyra auth frame")?;
                    let (response, key_index) = build_auth_server_notify(&logical, auth, session)?;
                    events(json!({
                        "event": "lyra_secure_channel_established",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "logical_id": format!("{:08X}", logical.logical_id),
                        "key_index": key_index,
                        "cipher": "P-256/HKDF-SHA256/AES-256-GCM",
                        "external_account_required": false,
                    }));
                    Ok(Some(response))
                }
                1 if logical.encrypted => {
                    let service = logical
                        .message
                        .as_deref()
                        .and_then(|message| bytes_field(message, 2).ok().flatten())
                        .and_then(|value| std::str::from_utf8(value).ok())
                        .unwrap_or_default();
                    let (response, peer_port) = build_connect_success_response(&logical, session)?;
                    if let Some((client_channel_id, server_channel_id, _)) = peer_port {
                        session.client_channel_id = Some(client_channel_id);
                        session.server_channel_id = Some(server_channel_id);
                    }
                    events(json!({
                        "event": "lyra_logical_connection_accepted",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "logical_id": format!("{:08X}", logical.logical_id),
                        "service": service,
                        "client_channel_id": peer_port.map(|value| value.0),
                        "server_channel_id": peer_port.map(|value| value.1),
                        "server_port": peer_port.map(|value| value.2),
                    }));
                    Ok(Some(response))
                }
                3 if logical.encrypted => {
                    events(json!({
                        "event": "lyra_logical_connection_confirmed",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "logical_id": format!("{:08X}", logical.logical_id),
                        "playable": false,
                    }));
                    Ok(None)
                }
                4 => Ok(None),
                _ => {
                    events(json!({
                        "event": "lyra_logical_frame",
                        "protocol": "xiaomi_miplay",
                        "peer": peer.to_string(),
                        "logical_id": format!("{:08X}", logical.logical_id),
                        "frame_type": logical.inner_type,
                        "encrypted": logical.encrypted,
                        "payload_hex": hex::encode(&logical.inner),
                    }));
                    Ok(None)
                }
            }
        }
        0x21 => {
            let (&channel_id, encrypted) = body
                .split_first()
                .context("missing Lyra channel identifier")?;
            let expected_channel_id = session.server_channel_id;
            let Some(session_key) = session.session_key.as_ref() else {
                events(json!({
                    "event": "lyra_channel_frame_rejected",
                    "protocol": "xiaomi_miplay",
                    "peer": peer.to_string(),
                    "channel_id": channel_id,
                    "reason": "missing_session_key",
                    "encrypted_hex": hex::encode(encrypted),
                }));
                return Ok(None);
            };

            match decrypt_payload(encrypted, session_key) {
                Ok(plain) => events(json!({
                    "event": "lyra_channel_frame_decrypted",
                    "protocol": "xiaomi_miplay",
                    "peer": peer.to_string(),
                    "channel_id": channel_id,
                    "expected_channel_id": expected_channel_id,
                    "client_channel_id": session.client_channel_id,
                    "payload_hex": hex::encode(&plain),
                    "payload_utf8": std::str::from_utf8(&plain).ok(),
                })),
                Err(error) => events(json!({
                    "event": "lyra_channel_frame_decrypt_failed",
                    "protocol": "xiaomi_miplay",
                    "peer": peer.to_string(),
                    "channel_id": channel_id,
                    "expected_channel_id": expected_channel_id,
                    "client_channel_id": session.client_channel_id,
                    "error": error.to_string(),
                    "encrypted_hex": hex::encode(encrypted),
                })),
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn transport_payload(frame: &[u8]) -> Result<(u8, &[u8])> {
    if frame.len() < 4 || frame[1] != 0x04 {
        bail!("invalid Lyra transport header");
    }
    let declared = u16::from_be_bytes([frame[2], frame[3]]) as usize;
    if declared != frame.len() {
        bail!(
            "invalid Lyra transport length: declared={declared}, actual={}",
            frame.len()
        );
    }
    Ok((frame[0], &frame[4..]))
}

fn physical_frame(body: &[u8]) -> Result<(u64, Option<&[u8]>)> {
    let frame = nested_bytes(body, &[2, 2])?.context("missing physical frame")?;
    let mut frame_type = 0;
    let mut message = None;
    for (field, value) in parse_fields(frame)? {
        match value {
            PbValue::Varint(value) if field == 2 => frame_type = value,
            PbValue::Bytes(value) if field >= 3 => message = Some(value),
            _ => {}
        }
    }
    Ok((frame_type, message))
}

fn parse_logical_frame(body: &[u8], key: Option<&[u8; 32]>) -> Result<LogicalFrame> {
    let outer = nested_bytes(body, &[2, 1])?.context("missing logical frame")?;
    let mut outer_scalars = Vec::new();
    let mut logical_id = 0;
    let mut encrypted = false;
    let mut payload = None;
    for (field, value) in parse_fields(outer)? {
        match value {
            PbValue::Varint(value) if field <= 4 => {
                if field == 3 {
                    logical_id = value;
                } else if field == 4 {
                    encrypted = value != 0;
                }
                outer_scalars.push((field, value));
            }
            PbValue::Bytes(value) if field == 5 => payload = Some(value),
            _ => {}
        }
    }
    let wire_payload = payload.context("missing logical inner frame")?;
    let inner = if encrypted {
        decrypt_payload(
            wire_payload,
            key.context("encrypted logical frame before ECDH")?,
        )?
    } else {
        wire_payload.to_vec()
    };
    let mut inner_type = 0;
    let mut message = None;
    for (field, value) in parse_fields(&inner)? {
        match value {
            PbValue::Varint(value) if field == 1 => inner_type = value,
            PbValue::Bytes(value) if field >= 2 => message = Some(value.to_vec()),
            _ => {}
        }
    }
    Ok(LogicalFrame {
        outer_scalars,
        logical_id,
        encrypted,
        inner_type,
        inner,
        message,
    })
}

fn build_device_sync_response(
    device_id: &str,
    name: &str,
    platform: &str,
    device_type: MiPlayDeviceType,
    account_hash: &[u8],
) -> Vec<u8> {
    let now = unix_millis();
    let detail = pb_join([pb_bytes(1, platform.as_bytes()), pb_bytes(2, b"FusionPlay")]);
    let mut device = Vec::new();
    pb_varint_into(&mut device, 1, now);
    pb_bytes_into(&mut device, 2, device_id.as_bytes());
    pb_bytes_into(&mut device, 4, account_hash);
    // HyperOS' DeviceInfo decoder reads the physical device type from field
    // 5. Field 3 is not the device type. This value uses Lyra's enum rather
    // than the Mi Connect enum advertised in `_mi-connect dev=...`.
    pb_varint_into(&mut device, 5, u64::from(device_type.lyra_protocol_value()));
    pb_bytes_into(&mut device, 6, name.as_bytes());
    pb_bytes_into(&mut device, 8, platform.as_bytes());
    pb_varint_into(&mut device, 9, 0x40080);
    pb_bytes_into(&mut device, 10, &detail);
    pb_bytes_into(&mut device, 11, b"5.1.251.10.fullCnRelease.0616209");
    pb_bytes_into(&mut device, 12, &pb_varint(1, 1));

    let mut response = Vec::new();
    pb_varint_into(&mut response, 1, now);
    pb_bytes_into(&mut response, 2, &device);
    pb_varint_into(&mut response, 3, 0x100);
    pb_varint_into(&mut response, 5, 0);
    let physical = pb_join([pb_varint(1, 1), pb_varint(2, 2), pb_bytes(4, &response)]);
    wrap_transport(0x09, &pb_bytes(2, &pb_bytes(2, &physical)))
}

fn build_keep_alive_response(request: &[u8]) -> Vec<u8> {
    let physical = pb_join([pb_varint(2, 5), pb_bytes(7, request)]);
    wrap_transport(0x09, &pb_bytes(2, &pb_bytes(2, &physical)))
}

fn build_logical_sync_response(logical: &LogicalFrame) -> Result<Vec<u8>> {
    let sync = logical
        .message
        .as_deref()
        .context("missing logical sync info")?;
    let timeout = varint_field(sync, 1)?.unwrap_or(10_000);
    let service = bytes_field(sync, 4)?.unwrap_or(b"com.milink.service:smartplay");
    let response_sync = pb_join([
        pb_varint(1, timeout),
        pb_varint(2, LOGICAL_TRUST_LOCAL_NETWORK),
        pb_bytes(4, service),
    ]);
    let response_inner = pb_join([pb_varint(1, 5), pb_bytes(6, &response_sync)]);
    Ok(wrap_logical(logical, &response_inner))
}

fn build_auth_server_notify(
    logical: &LogicalFrame,
    auth: &[u8],
    session: &mut PeerSession,
) -> Result<(Vec<u8>, u64)> {
    let key_index = varint_field(auth, 1)?.unwrap_or_default();
    let handshake = bytes_field(auth, 2)?.context("missing auth handshake")?;
    // HyperOS uses field 6 for the initial client notify, while the captured
    // MiPCAudio desktop flow can wrap the same key-agree frame in field 8.
    // Accept both wire variants before parsing the common payload.
    let key_agree = match bytes_field(handshake, 8)? {
        Some(value) => value,
        None => bytes_field(handshake, 6)?.context("missing key-agree frame")?,
    };
    let client_notify = nested_bytes(key_agree, &[2])?.context("missing client notify")?;
    let supported = nested_bytes(client_notify, &[1])?.context("missing cipher suites")?;
    let client_nonce = bytes_field(supported, 2)?.context("missing client nonce")?;
    let generic_key = bytes_field(supported, 5)?.context("missing generic public key")?;
    let client_public = bytes_field(generic_key, 2)?.context("missing P-256 public key")?;
    if client_nonce.len() != 32 || client_public.len() != 65 || client_public[0] != 4 {
        bail!("invalid P-256 client notify");
    }

    let peer_key = PublicKey::from_sec1_bytes(client_public).context("parse client P-256 key")?;
    let secret = EphemeralSecret::random(&mut OsRng);
    let server_public = EncodedPoint::from(secret.public_key());
    let shared = secret.diffie_hellman(&peer_key);
    let mut server_nonce = [0_u8; 32];
    OsRng.fill_bytes(&mut server_nonce);
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(client_nonce);
    info.extend_from_slice(&server_nonce);
    let hkdf = Hkdf::<Sha256>::new(
        Some(&SESSION_KEY_SALT),
        shared.raw_secret_bytes().as_slice(),
    );
    let mut session_key = [0_u8; 32];
    hkdf.expand(&info, &mut session_key)
        .map_err(|_| anyhow::anyhow!("derive Lyra session key"))?;
    session.session_key = Some(session_key);

    let generic_public = pb_join([pb_varint(1, 1), pb_bytes(2, server_public.as_bytes())]);
    let selected = pb_join([
        pb_varint(1, 1),
        pb_bytes(2, &server_nonce),
        pb_varint(3, 32),
        pb_varint(4, 2),
        pb_bytes(5, &generic_public),
    ]);
    let server_notify = pb_bytes(1, &selected);
    let key_agree_response = pb_join([pb_varint(1, 2), pb_bytes(3, &server_notify)]);
    let handshake_response = pb_join([
        pb_varint(1, 5),
        pb_varint(2, 6),
        pb_bytes(8, &key_agree_response),
    ]);
    let auth_response = pb_join([pb_varint(1, key_index), pb_bytes(2, &handshake_response)]);
    let inner = pb_join([pb_varint(1, 6), pb_bytes(7, &auth_response)]);
    Ok((wrap_logical(logical, &inner), key_index))
}

type PeerPortNegotiation = Option<(u64, u64, u16)>;

fn build_connect_success_response(
    logical: &LogicalFrame,
    session: &PeerSession,
) -> Result<(Vec<u8>, PeerPortNegotiation)> {
    let (connect_response, peer_port) = match logical.message.as_deref() {
        Some(request) => build_logical_connect_response(request)?,
        None => (Vec::new(), None),
    };
    let plain = pb_join([pb_varint(1, 2), pb_bytes(3, &connect_response)]);
    let encrypted = encrypt_payload(
        &plain,
        session
            .session_key
            .as_ref()
            .context("missing Lyra session key")?,
    )?;
    Ok((wrap_logical(logical, &encrypted), peer_port))
}

/// Builds the private-data portion of `LogiConnResponseFrame`.
///
/// HyperOS puts a `ChannelProto.RequestOfPeerPort` message in field 10 of
/// `UserInfoProto.UserInfo`.  Returning only status=0 makes Java call
/// `confirmChannel`, but the native channel remains in the negotiating state
/// until it receives `ResponseOfPeerPort` in UserInfo field 11.  The resulting
/// timeout is reported as 52008 and SystemUI removes the otherwise correctly
/// typed PC route.
fn build_logical_connect_response(request: &[u8]) -> Result<(Vec<u8>, PeerPortNegotiation)> {
    let Some(request_user_info) = bytes_field(request, 3)? else {
        return Ok((Vec::new(), None));
    };
    let Some(request_peer_port) = bytes_field(request_user_info, 10)? else {
        return Ok((
            pb_join([pb_varint(1, 0), pb_bytes(2, request_user_info)]),
            None,
        ));
    };
    let client_channel_id = varint_field(request_peer_port, 1)?
        .context("Lyra peer-port request is missing client channel id")?;
    // The native receiver allocates this independently from the client id.
    // FusionPlay currently exposes one control listener per process, so a
    // stable non-zero server channel id is sufficient and avoids leaking the
    // peer's identifier back as our own.
    let server_channel_id = 1_u64;
    let response_peer_port = pb_join([
        pb_varint(1, client_channel_id),
        pb_varint(2, server_channel_id),
        pb_varint(3, u64::from(CONTROL_PORT)),
        pb_varint(5, 1),
        pb_varint(6, 0),
    ]);

    let mut response_user_info = Vec::new();
    for (field, value) in parse_fields(request_user_info)? {
        // Field 10 is the request command.  A response must contain field 11
        // instead; retaining both causes some Continuity builds to keep
        // waiting for another negotiation round.
        if field == 10 || field == 11 {
            continue;
        }
        match value {
            PbValue::Varint(value) => pb_varint_into(&mut response_user_info, field, value),
            PbValue::Bytes(value) => pb_bytes_into(&mut response_user_info, field, value),
            PbValue::Fixed => {
                // Current UserInfo schemas use varints and length-delimited
                // fields only.  Fixed fields are deliberately not fabricated.
            }
        }
    }
    pb_bytes_into(&mut response_user_info, 11, &response_peer_port);

    let response = pb_join([pb_varint(1, 0), pb_bytes(2, &response_user_info)]);
    Ok((
        response,
        Some((client_channel_id, server_channel_id, CONTROL_PORT)),
    ))
}

fn wrap_logical(logical: &LogicalFrame, payload: &[u8]) -> Vec<u8> {
    let mut outer = Vec::new();
    for (field, value) in &logical.outer_scalars {
        pb_varint_into(&mut outer, *field, *value);
    }
    pb_bytes_into(&mut outer, 5, payload);
    wrap_transport(0x11, &pb_bytes(2, &pb_bytes(1, &outer)))
}

fn encrypt_payload(payload: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("initialize Lyra AES-GCM"))?;
    let mut nonce_bytes = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), payload)
        .map_err(|_| anyhow::anyhow!("encrypt Lyra logical payload"))?;
    let mut output = nonce_bytes.to_vec();
    output.extend_from_slice(&encrypted);
    Ok(output)
}

fn decrypt_payload(payload: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
    if payload.len() < 28 {
        bail!("encrypted Lyra payload is too short");
    }
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("initialize Lyra AES-GCM"))?;
    cipher
        .decrypt(Nonce::from_slice(&payload[..12]), &payload[12..])
        .map_err(|_| anyhow::anyhow!("decrypt Lyra logical payload"))
}

fn extract_phone_account(body: &[u8]) -> Result<Vec<u8>> {
    let request = nested_bytes(body, &[2, 2, 3])?.context("missing physical sync request")?;
    let device = nested_bytes(request, &[2])?.context("missing peer device info")?;
    Ok(bytes_field(device, 4)?.unwrap_or_default().to_vec())
}

fn nested_bytes<'a>(mut data: &'a [u8], path: &[u32]) -> Result<Option<&'a [u8]>> {
    for wanted in path {
        let Some(value) = bytes_field(data, *wanted)? else {
            return Ok(None);
        };
        data = value;
    }
    Ok(Some(data))
}

fn varint_field(data: &[u8], wanted: u32) -> Result<Option<u64>> {
    for (field, value) in parse_fields(data)? {
        if field == wanted
            && let PbValue::Varint(value) = value
        {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn bytes_field(data: &[u8], wanted: u32) -> Result<Option<&[u8]>> {
    for (field, value) in parse_fields(data)? {
        if field == wanted
            && let PbValue::Bytes(value) = value
        {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn parse_fields(data: &[u8]) -> Result<Vec<(u32, PbValue<'_>)>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let key = decode_varint(data, &mut offset)?;
        let field = (key >> 3) as u32;
        match (key & 7) as u8 {
            0 => result.push((field, PbValue::Varint(decode_varint(data, &mut offset)?))),
            1 => {
                if offset + 8 > data.len() {
                    bail!("truncated protobuf fixed64");
                }
                offset += 8;
                result.push((field, PbValue::Fixed));
            }
            2 => {
                let length = decode_varint(data, &mut offset)? as usize;
                if offset + length > data.len() {
                    bail!("truncated protobuf bytes");
                }
                result.push((field, PbValue::Bytes(&data[offset..offset + length])));
                offset += length;
            }
            5 => {
                if offset + 4 > data.len() {
                    bail!("truncated protobuf fixed32");
                }
                offset += 4;
                result.push((field, PbValue::Fixed));
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        }
    }
    Ok(result)
}

fn decode_varint(data: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    while *offset < data.len() {
        let byte = data[*offset];
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("protobuf varint is too long");
        }
    }
    bail!("truncated protobuf varint")
}

fn pb_varint(field: u32, value: u64) -> Vec<u8> {
    let mut output = Vec::new();
    pb_varint_into(&mut output, field, value);
    output
}

fn pb_varint_into(output: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(output, u64::from(field) << 3);
    encode_varint(output, value);
}

fn pb_bytes(field: u32, value: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    pb_bytes_into(&mut output, field, value);
    output
}

fn pb_bytes_into(output: &mut Vec<u8>, field: u32, value: &[u8]) {
    encode_varint(output, (u64::from(field) << 3) | 2);
    encode_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn pb_join<const N: usize>(parts: [Vec<u8>; N]) -> Vec<u8> {
    parts.concat()
}

fn encode_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn wrap_transport(kind: u8, body: &[u8]) -> Vec<u8> {
    let length = body.len() + 4;
    let mut output = Vec::with_capacity(length);
    output.push(kind);
    output.push(0x04);
    output.extend_from_slice(&(length as u16).to_be_bytes());
    output.extend_from_slice(body);
    output
}

fn kcp_header(conv: u32, cmd: u8, wnd: u16, ts: u32, sn: u32, una: u32, length: u32) -> Vec<u8> {
    let mut output = Vec::with_capacity(24);
    output.extend_from_slice(&conv.to_le_bytes());
    output.push(cmd);
    output.push(0);
    output.extend_from_slice(&wnd.to_le_bytes());
    output.extend_from_slice(&ts.to_le_bytes());
    output.extend_from_slice(&sn.to_le_bytes());
    output.extend_from_slice(&una.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output
}

fn monotonic_timestamp() -> u32 {
    unix_millis() as u32
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_length_includes_header() {
        let frame = wrap_transport(0x11, &[1, 2, 3]);
        assert_eq!(&frame[..4], &[0x11, 0x04, 0x00, 0x07]);
        assert_eq!(transport_payload(&frame).unwrap(), (0x11, &[1, 2, 3][..]));
    }

    #[test]
    fn aes_gcm_round_trip_uses_xiaomi_layout() {
        let key = [7_u8; 32];
        let encrypted = encrypt_payload(b"lyra", &key).unwrap();
        assert_eq!(encrypted.len(), 12 + 4 + 16);
        assert_eq!(decrypt_payload(&encrypted, &key).unwrap(), b"lyra");
    }

    #[test]
    fn successful_logical_response_keeps_oneof_set() {
        let request_peer_port = pb_join([
            pb_varint(1, 32),
            pb_bytes(4, &[4_u8; 32]),
            pb_bytes(5, &[5_u8; 32]),
        ]);
        let request_user_info = pb_join([
            pb_varint(1, 1),
            pb_bytes(2, b"com.milink.service"),
            pb_bytes(10, &request_peer_port),
        ]);
        let connect_request = pb_join([
            pb_varint(1, 1),
            pb_bytes(2, b"com.milink.service:smartplay"),
            pb_bytes(3, &request_user_info),
        ]);
        let logical = LogicalFrame {
            outer_scalars: vec![(1, 1), (2, 1), (3, 0x1234), (4, 1)],
            logical_id: 0x1234,
            encrypted: true,
            inner_type: 1,
            inner: Vec::new(),
            message: Some(connect_request),
        };
        let session = PeerSession {
            session_key: Some([3_u8; 32]),
            ..PeerSession::default()
        };
        let (response, peer_port) = build_connect_success_response(&logical, &session).unwrap();
        assert_eq!(peer_port, Some((32, 1, CONTROL_PORT)));
        let (_, body) = transport_payload(&response).unwrap();
        let parsed = parse_logical_frame(body, session.session_key.as_ref()).unwrap();
        assert_eq!(parsed.inner_type, 2);
        let connect_response = parsed.message.expect("logical connection response");
        assert_eq!(varint_field(&connect_response, 1).unwrap(), Some(0));
        let response_user_info = bytes_field(&connect_response, 2)
            .unwrap()
            .expect("response user info");
        assert_eq!(bytes_field(response_user_info, 10).unwrap(), None);
        assert_eq!(
            bytes_field(response_user_info, 2).unwrap(),
            Some(&b"com.milink.service"[..])
        );
        let response_peer_port = bytes_field(response_user_info, 11)
            .unwrap()
            .expect("peer-port response");
        assert_eq!(varint_field(response_peer_port, 1).unwrap(), Some(32));
        assert_eq!(varint_field(response_peer_port, 2).unwrap(), Some(1));
        assert_eq!(
            varint_field(response_peer_port, 3).unwrap(),
            Some(u64::from(CONTROL_PORT))
        );
        assert_eq!(varint_field(response_peer_port, 5).unwrap(), Some(1));
        assert_eq!(varint_field(response_peer_port, 6).unwrap(), Some(0));
    }

    #[test]
    fn physical_sync_preserves_selected_type_and_echoes_account_hash() {
        let account_hash = b"D5A7";
        for (device_type, expected_type) in [
            (MiPlayDeviceType::Vehicle, 8),
            (MiPlayDeviceType::Television, 3),
            (MiPlayDeviceType::Tablet, 2),
            (MiPlayDeviceType::Speaker, 0),
            (MiPlayDeviceType::DisplaySpeaker, 5),
        ] {
            let response = build_device_sync_response(
                "2433CD31",
                "ASUS",
                "Windows",
                device_type,
                account_hash,
            );
            let (kind, body) = transport_payload(&response).unwrap();
            assert_eq!(kind, 0x09);

            let sync = nested_bytes(body, &[2, 2, 4])
                .unwrap()
                .expect("physical sync response");
            let device = bytes_field(sync, 2).unwrap().expect("receiver device info");
            assert_eq!(varint_field(device, 3).unwrap(), None);
            assert_eq!(varint_field(device, 5).unwrap(), Some(expected_type),);
            assert_eq!(
                bytes_field(device, 4).unwrap(),
                Some(account_hash.as_slice())
            );
        }
    }
}
