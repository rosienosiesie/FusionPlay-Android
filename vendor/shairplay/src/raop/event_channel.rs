//! AirPlay 2 encrypted event and remote-control channels.
//!
//! After initial SETUP, the client connects to the event TCP port. Traffic is
//! encrypted with ChaCha20-Poly1305 using HKDF-derived keys. The channel is
//! bidirectional: receiver-originated commands travel out, while sender
//! capability updates arrive as RTSP requests and must be acknowledged.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::crypto::chacha_transport::EncryptedChannel;
use crate::error::{NetworkError, ProtocolError};
use crate::proto::http::{HttpRequest, HttpResponse};
use crate::raop::{Ap2RemoteControl, AudioHandler};

const MAX_EVENT_HEADER_BYTES: usize = 64 * 1024;
const MAX_EVENT_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Handle for sending commands through the event channel.
#[derive(Clone)]
pub(crate) struct EventSender {
    // Holding this sender keeps the outbound side alive for the RTSP
    // connection's lifetime. `send()` queues fully-framed plaintext RTSP;
    // EventChannel applies the event-channel encryption in wire order.
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl EventSender {
    /// Create from an existing channel sender.
    pub(crate) fn from_tx(tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self { tx }
    }

    /// Push a plaintext RTSP event to the controller over the encrypted AP2
    /// event channel.
    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), NetworkError> {
        self.tx
            .send(data)
            .map_err(|_| NetworkError::Mdns("event channel closed".into()))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InboundRtspMessage {
    Request(Vec<u8>),
    Response,
}

/// Frames a plaintext byte stream into complete RTSP requests and responses.
///
/// ChaCha transport blocks and TCP reads have no relationship to RTSP message
/// boundaries, so this buffer deliberately handles both fragmentation and
/// multiple messages coalesced into one decrypted chunk.
#[derive(Default)]
struct InboundRtspParser {
    buffer: Vec<u8>,
}

impl InboundRtspParser {
    fn push(&mut self, data: &[u8]) -> Result<Vec<InboundRtspMessage>, ProtocolError> {
        self.buffer.extend_from_slice(data);
        let mut messages = Vec::new();

        loop {
            let Some(header_end) = self.buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                if self.buffer.len() > MAX_EVENT_HEADER_BYTES {
                    return Err(ProtocolError::InvalidRtsp(
                        "event-channel RTSP headers exceed 64 KiB".into(),
                    ));
                }
                break;
            };
            let header_len = header_end + 4;
            if header_len > MAX_EVENT_HEADER_BYTES {
                return Err(ProtocolError::InvalidRtsp(
                    "event-channel RTSP headers exceed 64 KiB".into(),
                ));
            }

            let header = std::str::from_utf8(&self.buffer[..header_end])
                .map_err(|error| ProtocolError::InvalidRtsp(error.to_string()))?;
            let content_length = parse_content_length(header)?;
            if content_length > MAX_EVENT_BODY_BYTES {
                return Err(ProtocolError::InvalidRtsp(
                    "event-channel RTSP body exceeds 32 MiB".into(),
                ));
            }
            let message_len = header_len
                .checked_add(content_length)
                .ok_or_else(|| ProtocolError::InvalidRtsp("event-channel RTSP length overflow".into()))?;
            if self.buffer.len() < message_len {
                break;
            }

            let is_response = header
                .lines()
                .next()
                .map(|line| line.starts_with("RTSP/") || line.starts_with("HTTP/"))
                .unwrap_or(false);
            if is_response {
                self.buffer.drain(..message_len);
                messages.push(InboundRtspMessage::Response);
            } else {
                let message: Vec<u8> = self.buffer.drain(..message_len).collect();
                messages.push(InboundRtspMessage::Request(message));
            }
        }

        Ok(messages)
    }
}

fn parse_content_length(header: &str) -> Result<usize, ProtocolError> {
    let mut content_length = None;
    for line in header.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        let parsed = value
            .trim()
            .parse::<usize>()
            .map_err(|error| ProtocolError::InvalidRtsp(format!("invalid event-channel Content-Length: {error}")))?;
        if content_length.is_some_and(|previous| previous != parsed) {
            return Err(ProtocolError::InvalidRtsp(
                "conflicting event-channel Content-Length headers".into(),
            ));
        }
        content_length = Some(parsed);
    }
    Ok(content_length.unwrap_or(0))
}

fn parse_event_request(message: &[u8]) -> Result<HttpRequest, ProtocolError> {
    let mut request = HttpRequest::new();
    request.add_data(message)?;
    if !request.is_complete() {
        return Err(ProtocolError::InvalidRtsp(
            "incomplete framed event-channel RTSP request".into(),
        ));
    }
    Ok(request)
}

fn build_ok_response(request: &HttpRequest) -> Vec<u8> {
    let mut response = HttpResponse::new("RTSP/1.0", 200, "OK");
    response.add_header("CSeq", request.header("CSeq").unwrap_or("0"));
    response.finish(None);
    response.get_data().to_vec()
}

fn handle_inbound_message(
    message: InboundRtspMessage,
    remote: &Arc<Ap2RemoteControl>,
    handler: &dyn AudioHandler,
) -> Option<Vec<u8>> {
    let InboundRtspMessage::Request(message) = message else {
        debug!("Ignoring event-channel RTSP response");
        return None;
    };

    let request = match parse_event_request(&message) {
        Ok(request) => request,
        Err(error) => {
            warn!("Invalid event-channel RTSP request: {error}");
            return None;
        }
    };
    let method = request.method().unwrap_or("");
    let url = request.url().unwrap_or("");
    debug!(method, url, "Event-channel RTSP request");

    if method == "POST"
        && url == "/command"
        && let Some(data) = request.data()
    {
        crate::raop::handlers_ap2::apply_media_remote_command_update(data, remote, handler);
    }

    Some(build_ok_response(&request))
}

/// Async event channel that accepts one encrypted TCP connection.
pub(crate) struct EventChannel;

impl EventChannel {
    /// Handle a connected event channel stream.
    pub(crate) async fn handle_stream(
        stream: TcpStream,
        channel: EncryptedChannel,
        cmd_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        remote: Arc<Ap2RemoteControl>,
        handler: Arc<dyn AudioHandler>,
    ) {
        Self::handle(stream, channel, cmd_rx, remote, handler).await;
    }

    async fn handle(
        mut stream: TcpStream,
        mut channel: EncryptedChannel,
        mut cmd_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        remote: Arc<Ap2RemoteControl>,
        handler: Arc<dyn AudioHandler>,
    ) {
        let mut buf = vec![0u8; 4096];
        let mut encrypted_buf = Vec::new();
        let mut inbound_rtsp = InboundRtspParser::default();

        'event_loop: loop {
            tokio::select! {
                result = stream.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            debug!("Event channel closed by client");
                            break;
                        }
                        Ok(n) => {
                            encrypted_buf.extend_from_slice(&buf[..n]);
                            debug!(n, "Event channel data received");
                            let (plain, consumed) = match channel.decrypt_ctx.decrypt(&encrypted_buf) {
                                Ok(result) => result,
                                Err(error) => {
                                    warn!("Event channel decrypt error: {error}");
                                    break;
                                }
                            };
                            if consumed > 0 {
                                encrypted_buf.drain(..consumed);
                            }
                            if plain.is_empty() {
                                continue;
                            }

                            debug!(len = plain.len(), "Event channel plaintext received");
                            let messages = match inbound_rtsp.push(&plain) {
                                Ok(messages) => messages,
                                Err(error) => {
                                    warn!("Event channel RTSP framing error: {error}");
                                    break;
                                }
                            };
                            for message in messages {
                                let Some(response) =
                                    handle_inbound_message(message, &remote, handler.as_ref())
                                else {
                                    continue;
                                };
                                let encrypted = match channel.encrypt_ctx.encrypt(&response) {
                                    Ok(encrypted) => encrypted,
                                    Err(error) => {
                                        warn!("Event channel response encrypt error: {error}");
                                        break 'event_loop;
                                    }
                                };
                                if let Err(error) = stream.write_all(&encrypted).await {
                                    warn!("Event channel response write error: {error}");
                                    break 'event_loop;
                                }
                            }
                        }
                        Err(error) => {
                            warn!("Event channel read error: {error}");
                            break;
                        }
                    }
                }
                Some(data) = cmd_rx.recv() => {
                    debug!(len = data.len(), "Sending on event channel");
                    let encrypted = match channel.encrypt_ctx.encrypt(&data) {
                        Ok(encrypted) => encrypted,
                        Err(error) => {
                            warn!("Event channel encrypt error: {error}");
                            break;
                        }
                    };
                    if let Err(error) = stream.write_all(&encrypted).await {
                        warn!("Event channel write error: {error}");
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{
        InboundRtspMessage, InboundRtspParser, build_ok_response, handle_inbound_message, parse_event_request,
    };
    use crate::raop::{Ap2RemoteControl, AudioFormat, AudioHandler, AudioSession, RemoteCommand, RemoteControl};

    struct NoopAudioSession;

    impl AudioSession for NoopAudioSession {
        fn audio_process(&mut self, _samples: &[f32]) {}
    }

    #[derive(Default)]
    struct CapturingAudioHandler {
        updates: Mutex<Vec<Vec<RemoteCommand>>>,
    }

    impl AudioHandler for CapturingAudioHandler {
        fn audio_init(&self, _format: AudioFormat) -> Box<dyn AudioSession> {
            Box::new(NoopAudioSession)
        }

        fn on_remote_control(&self, remote: Arc<dyn RemoteControl>) {
            self.updates.lock().unwrap().push(remote.available_commands());
        }
    }

    fn supported_command(command_id: i64) -> plist::Value {
        let mut info = plist::Dictionary::new();
        info.insert(
            "kCommandInfoCommandKey".into(),
            plist::Value::Integer(command_id.into()),
        );
        info.insert("kCommandInfoEnabledKey".into(), plist::Value::Boolean(true));
        let mut encoded = Vec::new();
        plist::to_writer_binary(&mut encoded, &info).unwrap();
        plist::Value::Data(encoded)
    }

    #[test]
    fn frames_fragmented_and_coalesced_rtsp_requests() {
        let body = b"ABCD";
        let first = format!(
            "POST /command RTSP/1.0\r\nCSeq: 11\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let mut first_with_body = first;
        first_with_body.extend_from_slice(body);
        let second = b"OPTIONS * RTSP/1.0\r\nCSeq: 12\r\n\r\n";

        let split = first_with_body.len() - 2;
        let mut parser = InboundRtspParser::default();
        assert!(parser.push(&first_with_body[..split]).unwrap().is_empty());

        let mut remainder = first_with_body[split..].to_vec();
        remainder.extend_from_slice(second);
        let messages = parser.push(&remainder).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], InboundRtspMessage::Request(first_with_body));
        assert_eq!(messages[1], InboundRtspMessage::Request(second.to_vec()));
    }

    #[test]
    fn frames_responses_without_treating_them_as_requests() {
        let response = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nContent-Length: 4\r\n\r\nPONG".to_vec();
        let mut parser = InboundRtspParser::default();

        assert_eq!(parser.push(&response).unwrap(), vec![InboundRtspMessage::Response]);
    }

    #[test]
    fn ok_response_echoes_request_cseq() {
        let request = parse_event_request(b"POST /command RTSP/1.0\r\nCSeq: 42\r\nContent-Length: 0\r\n\r\n").unwrap();
        let response = String::from_utf8(build_ok_response(&request)).unwrap();

        assert!(response.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(response.contains("\r\nCSeq: 42\r\n"));
        assert!(response.ends_with("\r\n\r\n"));
    }

    #[test]
    fn command_request_updates_capabilities_and_is_acknowledged() {
        let mut params = plist::Dictionary::new();
        params.insert(
            "mrSupportedCommandsFromSender".into(),
            plist::Value::Array(vec![supported_command(2)]),
        );
        let mut update = plist::Dictionary::new();
        update.insert("type".into(), plist::Value::String("updateMRSupportedCommands".into()));
        update.insert("params".into(), plist::Value::Dictionary(params));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &update).unwrap();

        let mut request = format!(
            "POST /command RTSP/1.0\r\nCSeq: 73\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Arc::new(Ap2RemoteControl::new(
            super::EventSender::from_tx(tx),
            "3AADED63-D2CD-4B2D-95A3-A3E6B8520DCD".into(),
            "Windows".into(),
        ));
        let handler = CapturingAudioHandler::default();

        let response = handle_inbound_message(InboundRtspMessage::Request(request), &remote, &handler).unwrap();

        assert_eq!(remote.available_commands(), vec![RemoteCommand::PlayPause]);
        assert_eq!(*handler.updates.lock().unwrap(), vec![vec![RemoteCommand::PlayPause]]);
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(response.contains("\r\nCSeq: 73\r\n"));
    }

    #[test]
    fn rejects_conflicting_content_lengths() {
        let mut parser = InboundRtspParser::default();
        let error = parser
            .push(b"POST /command RTSP/1.0\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nAB")
            .unwrap_err();

        assert!(error.to_string().contains("conflicting"));
    }
}
