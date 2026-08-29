//! DACP (Digital Audio Control Protocol) client for remote-controlling Apple devices.
//!
//! When an iPhone/iPad/Mac streams audio via AirPlay, it advertises a `_dacp._tcp` mDNS
//! service. This module discovers that service and sends HTTP commands back to control
//! playback (play/pause, next, previous, volume, etc.).

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::time::Duration;

use crate::error::NetworkError;
use tracing::debug;

/// Default DACP port, used when mDNS discovery of the `_dacp._tcp` service fails.
const DACP_DEFAULT_PORT: u16 = 3689;

const PLAY_PATH: &str = "/ctrl-int/1/play";
const PAUSE_PATH: &str = "/ctrl-int/1/pause";
const PLAY_PAUSE_PATH: &str = "/ctrl-int/1/playpause";
const NEXT_PATH: &str = "/ctrl-int/1/nextitem";
const PREVIOUS_PATH: &str = "/ctrl-int/1/previtem";
const STOP_PATH: &str = "/ctrl-int/1/stop";

fn volume_path(volume: u8) -> String {
    format!("/ctrl-int/1/setproperty?dmcp.volume={}", volume.min(100))
}

fn shuffle_path(on: bool) -> String {
    let state = if on { 1 } else { 0 };
    format!("/ctrl-int/1/setproperty?dacp.shufflestate={state}")
}

fn repeat_path(state: u8) -> String {
    format!("/ctrl-int/1/setproperty?dacp.repeatstate={state}")
}

fn seek_path(position_ms: u64) -> String {
    format!("/ctrl-int/1/setproperty?dacp.playingtime={position_ms}")
}

/// Browse `_dacp._tcp` via mDNS and return the port for the given DACP-ID.
/// Returns None if not found within 2 seconds.
#[cfg(not(target_os = "macos"))]
fn discover_dacp_port(dacp_id: &str, _remote_ip: std::net::IpAddr) -> Option<u16> {
    let daemon = mdns_sd::ServiceDaemon::new().ok()?;
    let receiver = daemon.browse("_dacp._tcp.local.").ok()?;
    let target = dacp_id.to_uppercase();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);

    while std::time::Instant::now() < deadline {
        match receiver.recv_timeout(deadline.duration_since(std::time::Instant::now())) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                if info.get_fullname().to_uppercase().contains(&target) {
                    let port = info.get_port();
                    let _ = daemon.shutdown();
                    return Some(port);
                }
            }
            Err(_) => break,
            _ => continue,
        }
    }
    let _ = daemon.shutdown();
    None
}

/// Browse `_dacp._tcp` via Bonjour and return the port for the given DACP-ID.
/// Always returns None on macOS — astro-dnssd doesn't expose a synchronous
/// browse+resolve API. The caller falls back to port 3689.
#[cfg(target_os = "macos")]
fn discover_dacp_port(dacp_id: &str, _remote_ip: std::net::IpAddr) -> Option<u16> {
    let _ = dacp_id;
    None
}

/// Client for sending DACP remote control commands to an Apple device.
///
/// Created from the DACP ID and Active-Remote header received by the AirPlay session.
///
/// # Example
/// ```text
/// let mut client = DacpClient::new("7711DA8B47838CB5", "1986535575");
/// client.discover_from_remote("192.168.1.5".parse().unwrap());
/// // Then from a synchronous remote-control callback:
/// // client.play_pause_blocking().ok();
/// ```
/// HTTP client for sending DACP playback commands to the iPhone.
#[derive(Debug)]
pub(crate) struct DacpClient {
    /// DACP-ID from the RTSP session. Identifies the `_dacp._tcp` mDNS service.
    dacp_id: String,
    active_remote: String,
    addr: Option<SocketAddr>,
}

impl DacpClient {
    /// Create a new DACP client from the values received in the AirPlay session.
    pub(crate) fn new(dacp_id: &str, active_remote: &str) -> Self {
        Self {
            dacp_id: dacp_id.to_string(),
            active_remote: active_remote.to_string(),
            addr: None,
        }
    }

    /// Discover the Apple device's DACP service via mDNS.
    ///
    /// Browses `_dacp._tcp.local.` for a service matching the DACP-ID,
    /// with a 2-second timeout. Falls back to port 3689 on the remote IP
    /// if mDNS discovery fails.
    pub(crate) fn discover_from_remote(&mut self, remote_ip: std::net::IpAddr) {
        self.addr = match discover_dacp_port(&self.dacp_id, remote_ip) {
            Some(port) => {
                debug!(port, dacp_id = %self.dacp_id, "DACP service discovered via mDNS");
                Some(SocketAddr::new(remote_ip, port))
            }
            None => {
                debug!(dacp_id = %self.dacp_id, "DACP mDNS discovery failed, falling back to port 3689");
                Some(SocketAddr::new(remote_ip, DACP_DEFAULT_PORT))
            }
        };
    }

    /// Send a raw DACP command from synchronous callbacks.
    pub(crate) fn command_blocking(&self, path: &str) -> Result<(), NetworkError> {
        let addr = self
            .addr
            .ok_or_else(|| NetworkError::Mdns("DACP not discovered yet — call discover() first".into()))?;

        let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let request = self.command_request(path, addr);
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut status_line = String::new();
        BufReader::new(stream).read_line(&mut status_line)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid DACP HTTP status line: {status_line:?}"),
                )
            })?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("DACP command returned HTTP {status}")).into())
        }
    }

    pub(crate) fn play_pause_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(PLAY_PAUSE_PATH)
    }

    pub(crate) fn play_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(PLAY_PATH)
    }

    pub(crate) fn pause_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(PAUSE_PATH)
    }

    pub(crate) fn next_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(NEXT_PATH)
    }

    pub(crate) fn prev_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(PREVIOUS_PATH)
    }

    pub(crate) fn stop_blocking(&self) -> Result<(), NetworkError> {
        self.command_blocking(STOP_PATH)
    }

    pub(crate) fn set_volume_blocking(&self, volume: u8) -> Result<(), NetworkError> {
        self.command_blocking(&volume_path(volume))
    }

    pub(crate) fn set_shuffle_blocking(&self, on: bool) -> Result<(), NetworkError> {
        self.command_blocking(&shuffle_path(on))
    }

    pub(crate) fn set_repeat_blocking(&self, state: u8) -> Result<(), NetworkError> {
        self.command_blocking(&repeat_path(state))
    }

    pub(crate) fn seek_blocking(&self, position_ms: u64) -> Result<(), NetworkError> {
        self.command_blocking(&seek_path(position_ms))
    }

    fn command_request(&self, path: &str, addr: SocketAddr) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nActive-Remote: {}\r\nHost: {addr}\r\nConnection: close\r\n\r\n",
            self.active_remote
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};

    use super::DacpClient;

    fn serve_once(response: &'static [u8]) -> (SocketAddr, Arc<Mutex<Vec<u8>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_for_thread = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 256];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
            }
            *captured_for_thread.lock().unwrap() = request;
            stream.write_all(response).unwrap();
        });
        (addr, captured, handle)
    }

    fn client_for(addr: SocketAddr) -> DacpClient {
        DacpClient {
            dacp_id: "test".into(),
            active_remote: "123456789".into(),
            addr: Some(addr),
        }
    }

    #[test]
    fn play_pause_uses_dacp_path_and_active_remote() {
        let (addr, captured, handle) = serve_once(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        client_for(addr).play_pause_blocking().unwrap();
        handle.join().unwrap();
        let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();

        assert!(request.starts_with("GET /ctrl-int/1/playpause HTTP/1.1\r\n"));
        assert!(request.contains("\r\nActive-Remote: 123456789\r\n"));
        assert!(request.contains("\r\nConnection: close\r\n"));
    }

    #[test]
    fn explicit_pause_uses_pause_path() {
        let (addr, captured, handle) = serve_once(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        client_for(addr).pause_blocking().unwrap();
        handle.join().unwrap();
        let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();

        assert!(request.starts_with("GET /ctrl-int/1/pause HTTP/1.1\r\n"));
    }

    #[test]
    fn seek_uses_absolute_dacp_playing_time() {
        let (addr, captured, handle) = serve_once(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        client_for(addr).seek_blocking(91_250).unwrap();
        handle.join().unwrap();
        let request = String::from_utf8(captured.lock().unwrap().clone()).unwrap();

        assert!(request.starts_with("GET /ctrl-int/1/setproperty?dacp.playingtime=91250 HTTP/1.1\r\n"));
    }

    #[test]
    fn non_success_status_is_not_reported_as_sent() {
        let (addr, _, handle) = serve_once(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        let error = client_for(addr).play_pause_blocking().unwrap_err();
        handle.join().unwrap();

        assert!(error.to_string().contains("HTTP 403"));
    }
}
