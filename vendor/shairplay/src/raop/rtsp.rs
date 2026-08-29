//! RTSP request dispatch — routes incoming requests to handlers.
//!
//! Uses a compile-time route table for clean, extensible routing.
//! Auth, Apple-Challenge, and logging are handled as middleware
//! before dispatch.

use crate::proto::digest;
use crate::proto::http::{HttpRequest, HttpResponse};
use crate::raop::handlers_ap1::{self as handlers, RaopConnection};

/// HTTP Digest authentication realm advertised and validated for RTSP auth.
const DIGEST_REALM: &str = "airplay";
#[cfg(feature = "ap2")]
use crate::raop::handlers_ap2;
#[cfg(feature = "hls")]
use crate::raop::handlers_hls;

/// Handler function signature — all RTSP handlers share this type.
type Handler = fn(&mut RaopConnection, &HttpRequest, &mut HttpResponse) -> Option<Vec<u8>>;

/// Result of route resolution.
enum RouteResolution {
    /// Request is handled inline and has no body.
    NoBody,
    /// Request should be passed to a handler function.
    Handler(Handler),
}

/// A single route entry: HTTP method, URL path, handler function.
struct Route {
    method: &'static str,
    path: &'static str,
    handler: Handler,
}

/// Static route table — checked in order, first match wins.
/// Feature-gated routes are included/excluded at compile time.
const ROUTES: &[Route] = &[
    // --- Authentication & DRM ---
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/pair-setup",
        handler: handlers_ap2::handle_pair_setup,
    },
    #[cfg(not(feature = "ap2"))]
    Route {
        method: "POST",
        path: "/pair-setup",
        handler: handlers::handle_pair_setup,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/pair-verify",
        handler: handlers_ap2::handle_pair_verify,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/pair-pin-start",
        handler: handlers_ap2::handle_pair_pin_start,
    },
    #[cfg(not(feature = "ap2"))]
    Route {
        method: "POST",
        path: "/pair-verify",
        handler: handlers::handle_pair_verify,
    },
    Route {
        method: "POST",
        path: "/fp-setup",
        handler: handlers::handle_fp_setup,
    },
    // --- AP2 POST endpoints ---
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/feedback",
        handler: handlers_ap2::handle_feedback,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/command",
        handler: handlers_ap2::handle_command,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "POST",
        path: "/audioMode",
        handler: handlers_ap2::handle_audio_mode,
    },
    // --- Standard RTSP methods ---
    Route {
        method: "OPTIONS",
        path: "*",
        handler: handlers::handle_options,
    },
    Route {
        method: "ANNOUNCE",
        path: "*",
        handler: handlers::handle_announce,
    },
    Route {
        method: "GET_PARAMETER",
        path: "*",
        handler: handlers::handle_get_parameter,
    },
    Route {
        method: "SET_PARAMETER",
        path: "*",
        handler: handlers::handle_set_parameter,
    },
    Route {
        method: "PAUSE",
        path: "*",
        handler: handlers::handle_pause,
    },
    // --- AP2 RTSP methods ---
    #[cfg(feature = "ap2")]
    Route {
        method: "SETRATEANCHORTIME",
        path: "*",
        handler: handlers_ap2::handle_set_rate_anchor_time,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "SETPEERS",
        path: "*",
        handler: handlers_ap2::handle_set_peers,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "SETPEERSX",
        path: "*",
        handler: handlers_ap2::handle_set_peers,
    },
    #[cfg(feature = "ap2")]
    Route {
        method: "FLUSHBUFFERED",
        path: "*",
        handler: handlers_ap2::handle_flush_buffered,
    },
    // --- Info ---
    #[cfg(feature = "ap2")]
    Route {
        method: "GET",
        path: "/info",
        handler: handlers_ap2::handle_info,
    },
    // --- HLS (HTTP Live Streaming) ---
    #[cfg(feature = "hls")]
    Route {
        method: "GET",
        path: "/server-info",
        handler: handlers_hls::handle_server_info,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "POST",
        path: "/play",
        handler: handlers_hls::handle_play,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "GET",
        path: "/playback-info",
        handler: handlers_hls::handle_playback_info,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "POST",
        path: "/stop",
        handler: handlers_hls::handle_stop,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "POST",
        path: "/scrub",
        handler: handlers_hls::handle_scrub,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "GET",
        path: "/scrub",
        handler: handlers_hls::handle_scrub_info,
    },
    #[cfg(feature = "hls")]
    Route {
        method: "POST",
        path: "/rate",
        handler: handlers_hls::handle_rate,
    },
];

/// Dispatch an RTSP request: authenticate, resolve route, call handler, build response.
pub(crate) fn dispatch(conn: &mut RaopConnection, request: &HttpRequest) -> HttpResponse {
    let method = request.method().unwrap_or("");
    let url = request.url().unwrap_or("");
    let cseq = request.header("CSeq").unwrap_or("0");
    let response_protocol = if cfg!(feature = "hls") && is_hls_http_endpoint(url) {
        "HTTP/1.1"
    } else {
        "RTSP/1.0"
    };

    let mut response = HttpResponse::new(response_protocol, 200, "OK");
    response.add_header("CSeq", cseq);
    response.add_header("Apple-Jack-Status", "connected; type=analog");

    // --- Middleware: authentication ---
    if method != "OPTIONS" && !conn.shared.password.is_empty() {
        let authorization = request.header("Authorization");
        if !digest::is_valid(
            DIGEST_REALM,
            &conn.shared.password,
            &conn.nonce,
            method,
            url,
            authorization,
        ) {
            let auth_str = format!("Digest realm=\"{}\", nonce=\"{}\"", DIGEST_REALM, conn.nonce);
            response = HttpResponse::new(response_protocol, 401, "Unauthorized");
            response.add_header("CSeq", cseq);
            response.add_header("WWW-Authenticate", &auth_str);
            response.finish(None);
            return response;
        }
    }

    #[cfg(feature = "ap2")]
    handlers::capture_dacp_remote_control(conn, request);

    // --- Middleware: Apple-Challenge ---
    if let Some(challenge) = request.header("Apple-Challenge")
        && let Ok(sig) = conn
            .shared
            .rsakey
            .sign_challenge(challenge, &conn.local_addr, &conn.shared.hwaddr)
    {
        response.add_header("Apple-Response", &sig);
    }

    // --- Route resolution ---
    let response_data = match resolve_handler(conn, request, method, url) {
        Some(RouteResolution::Handler(handler)) => handler(conn, request, &mut response),
        Some(RouteResolution::NoBody) => None,
        None => {
            tracing::debug!(method, url, "Unhandled RTSP request");
            response = HttpResponse::new(response_protocol, 404, "Not Found");
            response.add_header("CSeq", cseq);
            response.finish(None);
            return response;
        }
    };
    response.finish(response_data.as_deref());
    response
}

fn is_hls_http_endpoint(url: &str) -> bool {
    matches!(
        url.split('?').next().unwrap_or(url),
        "/server-info" | "/play" | "/playback-info" | "/stop" | "/scrub" | "/rate"
    )
}

/// Resolve the handler for a request. Checks the route table first,
/// then falls back to special-case handlers for methods that need
/// custom routing logic (SETUP, RECORD, FLUSH, TEARDOWN).
fn resolve_handler(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    method: &str,
    url: &str,
) -> Option<RouteResolution> {
    // 1. Check static route table (exact path or prefix match for query-string routes)
    for route in ROUTES {
        if route.method == method {
            let path = url.split('?').next().unwrap_or(url);
            if route.path == "*" || route.path == path {
                return Some(RouteResolution::Handler(route.handler));
            }
        }
    }

    // 2. Special-case methods with custom routing logic
    match method {
        "SETUP" => resolve_setup(conn, request).map(RouteResolution::Handler),
        "RECORD" => resolve_record(conn).map(RouteResolution::Handler),
        "FLUSH" => {
            handle_flush_inline(conn, request);
            Some(RouteResolution::NoBody)
        }
        "TEARDOWN" => Some(RouteResolution::Handler(handle_teardown as Handler)),
        _ => None,
    }
}

/// SETUP routing: AP1 (Transport header) vs AP2 (binary plist body).
fn resolve_setup(conn: &RaopConnection, request: &HttpRequest) -> Option<Handler> {
    #[cfg(feature = "ap2")]
    {
        let is_plist = request.data().map(|d| d.starts_with(b"bplist")).unwrap_or(false);
        if conn.is_ap2 || is_plist {
            return Some(handlers_ap2::handle_setup);
        }
    }
    let _ = (conn, request); // suppress unused warnings without ap2
    Some(handlers::handle_setup)
}

/// RECORD routing: AP2 has its own handler.
fn resolve_record(conn: &RaopConnection) -> Option<Handler> {
    #[cfg(feature = "ap2")]
    if conn.is_ap2 {
        return Some(handlers_ap2::handle_record);
    }
    let _ = conn;
    Some(handlers::handle_record)
}

/// FLUSH: parse RTP-Info header and flush the buffer inline.
fn handle_flush_inline(conn: &mut RaopConnection, request: &HttpRequest) {
    if let Some(rtp) = &conn.raop_rtp {
        let next_seq = request
            .header("RTP-Info")
            .and_then(parse_rtp_info_sequence)
            .map(i32::from)
            .unwrap_or(-1);
        rtp.flush(next_seq);
    }
}

fn parse_rtp_info_sequence(value: &str) -> Option<u16> {
    value.split(';').find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("seq") {
            return None;
        }
        value.trim().parse::<u16>().ok()
    })
}

#[cfg(test)]
mod classic_rtp_info_tests {
    use super::parse_rtp_info_sequence;

    #[test]
    fn standard_flush_header_extracts_sequence_before_rtptime() {
        assert_eq!(parse_rtp_info_sequence("seq=25009;rtptime=1148010660"), Some(25_009));
    }

    #[test]
    fn sequence_is_order_independent_and_case_insensitive() {
        assert_eq!(
            parse_rtp_info_sequence("rtptime=1148010660; SeQ = 65535"),
            Some(u16::MAX)
        );
    }

    #[test]
    fn missing_or_out_of_range_sequence_is_rejected() {
        assert_eq!(parse_rtp_info_sequence("rtptime=1148010660"), None);
        assert_eq!(parse_rtp_info_sequence("seq=65536;rtptime=1"), None);
    }
}

#[cfg(feature = "ap2")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ap2TeardownScope {
    AudioStream,
    AuxiliaryStream,
    Connection,
    Ignore,
}

#[cfg(feature = "ap2")]
fn ap2_teardown_scope(data: Option<&[u8]>) -> Ap2TeardownScope {
    let Some(data) = data else {
        return Ap2TeardownScope::Ignore;
    };
    let Ok(plist) = plist::from_bytes::<plist::Value>(data) else {
        return Ap2TeardownScope::Ignore;
    };
    let Some(dict) = plist.as_dictionary() else {
        return Ap2TeardownScope::Ignore;
    };
    let Some(streams) = dict.get("streams") else {
        return Ap2TeardownScope::Connection;
    };
    let Some(streams) = streams.as_array() else {
        return Ap2TeardownScope::Ignore;
    };

    // Audio is carried by stream types 96 (realtime) and 103 (buffered).
    // Type 130 is MediaRemote and type 110 is video.  Their teardown is
    // independent of the audio playout session and must not silence it.
    if streams.iter().any(|stream| {
        stream
            .as_dictionary()
            .and_then(|stream| stream.get("type"))
            .and_then(plist::Value::as_unsigned_integer)
            .is_some_and(|stream_type| matches!(stream_type, 96 | 103))
    }) {
        Ap2TeardownScope::AudioStream
    } else {
        Ap2TeardownScope::AuxiliaryStream
    }
}

fn stop_audio_stream(conn: &mut RaopConnection) {
    if let Some(mut rtp) = conn.raop_rtp.take() {
        rtp.stop();
    }
    #[cfg(feature = "ap2")]
    {
        let stopped_active = conn.shared.stop_active_audio_owned_by(conn.close_handle.id());
        if !stopped_active && let Some(cmd) = conn.playout_cmd.as_ref() {
            let _ = cmd.send(crate::raop::buffered_audio::PlayoutCommand::Stop);
        }
        conn.playout_cmd = None;
    }
}

/// TEARDOWN: an AP2 `streams` plist only stops that stream. Classic AirPlay and
/// an AP2 connection-scoped teardown close the RTSP connection.
fn handle_teardown(conn: &mut RaopConnection, request: &HttpRequest, response: &mut HttpResponse) -> Option<Vec<u8>> {
    #[cfg(feature = "ap2")]
    if conn.is_ap2 {
        let scope = ap2_teardown_scope(request.data());
        tracing::info!(
            ?scope,
            connection_id = conn.close_handle.id(),
            "AP2 TEARDOWN classified"
        );
        match scope {
            Ap2TeardownScope::AudioStream => {
                stop_audio_stream(conn);
                return None;
            }
            Ap2TeardownScope::AuxiliaryStream => return None,
            Ap2TeardownScope::Ignore => return None,
            Ap2TeardownScope::Connection => {}
        }
    }

    stop_audio_stream(conn);
    response.add_header("Connection", "close");
    response.set_disconnect(true);
    None
}

#[cfg(all(test, feature = "ap2"))]
mod teardown_tests {
    use super::{Ap2TeardownScope, ap2_teardown_scope};

    fn plist_body(value: plist::Value) -> Vec<u8> {
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &value).unwrap();
        body
    }

    fn stream(stream_type: u64) -> plist::Value {
        let mut stream = plist::Dictionary::new();
        stream.insert("type".into(), plist::Value::Integer(stream_type.into()));
        plist::Value::Dictionary(stream)
    }

    fn streams_body(streams: Vec<plist::Value>) -> Vec<u8> {
        let mut dict = plist::Dictionary::new();
        dict.insert("streams".into(), plist::Value::Array(streams));
        plist_body(plist::Value::Dictionary(dict))
    }

    #[test]
    fn realtime_and_buffered_audio_teardowns_stop_audio_only() {
        for stream_type in [96, 103] {
            let body = streams_body(vec![stream(stream_type)]);
            assert_eq!(ap2_teardown_scope(Some(&body)), Ap2TeardownScope::AudioStream);
        }
    }

    #[test]
    fn mediaremote_video_and_empty_stream_teardowns_preserve_audio() {
        for streams in [vec![stream(130)], vec![stream(110)], Vec::new()] {
            let body = streams_body(streams);
            assert_eq!(ap2_teardown_scope(Some(&body)), Ap2TeardownScope::AuxiliaryStream);
        }
    }

    #[test]
    fn mixed_stream_teardown_stops_audio() {
        let body = streams_body(vec![stream(130), stream(96)]);
        assert_eq!(ap2_teardown_scope(Some(&body)), Ap2TeardownScope::AudioStream);
    }

    #[test]
    fn connection_scoped_teardown_is_distinct() {
        let mut dict = plist::Dictionary::new();
        dict.insert("sessionUUID".into(), plist::Value::String("session".into()));
        let body = plist_body(plist::Value::Dictionary(dict));

        assert_eq!(ap2_teardown_scope(Some(&body)), Ap2TeardownScope::Connection);
    }

    #[test]
    fn missing_or_invalid_ap2_teardown_body_is_ignored() {
        assert_eq!(ap2_teardown_scope(None), Ap2TeardownScope::Ignore);
        assert_eq!(ap2_teardown_scope(Some(b"not a plist")), Ap2TeardownScope::Ignore);

        let mut dict = plist::Dictionary::new();
        dict.insert("streams".into(), plist::Value::String("invalid".into()));
        let body = plist_body(plist::Value::Dictionary(dict));
        assert_eq!(ap2_teardown_scope(Some(&body)), Ap2TeardownScope::Ignore);
    }
}
