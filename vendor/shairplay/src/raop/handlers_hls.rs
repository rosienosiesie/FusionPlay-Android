//! HLS HTTP handlers — /play, /playback-info, /scrub, /rate, /stop, /server-info.

use super::handlers_ap1::RaopConnection;
use crate::proto::http::{HttpRequest, HttpResponse};

/// `GET /server-info` — server capabilities for HLS mode.
pub(crate) fn handle_server_info(
    conn: &mut RaopConnection,
    _request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let mac = conn
        .shared
        .hwaddr
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");

    let mut dict = plist::Dictionary::new();
    // Bits 0-6 + 9: video, photo, FairPlay DRM, volume, HLS, slideshow, unknown, audio
    dict.insert("features".into(), plist::Value::Integer(0x27F_i64.into()));
    dict.insert("macAddress".into(), plist::Value::String(mac.clone()));
    dict.insert(
        "model".into(),
        plist::Value::String(crate::raop::config::GLOBAL_MODEL.into()),
    );
    dict.insert("osBuildVersion".into(), plist::Value::String("12B435".into()));
    dict.insert("protovers".into(), plist::Value::String("1.0".into()));
    dict.insert(
        "srcvers".into(),
        plist::Value::String(crate::raop::config::AP2_SRCVERS.into()),
    );
    dict.insert("vv".into(), plist::Value::Integer(2_i64.into()));
    dict.insert("deviceid".into(), plist::Value::String(mac));

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(dict)).ok()?;
    response.add_header("Content-Type", "text/x-apple-plist+xml");
    Some(buf)
}

/// `POST /play` — iPhone sends m3u8 URL to start HLS playback.
pub(crate) fn handle_play(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let data = request.data()?;
    let (url, start_pos) = parse_play_body(data)?;

    let session_id = request.header("X-Apple-Session-ID").map(|s| s.to_string());

    tracing::info!(start_pos, "HLS play request");

    let hls_handler = conn.shared.hls_handler.as_ref()?;
    let (mut previous_session, previous_owner) = conn
        .hls_state
        .lock()
        .ok()
        .map(|mut state| {
            state.session_id = None;
            (state.session.take(), state.owner.take())
        })
        .unwrap_or((None, None));
    if let Some(previous) = previous_session.as_mut() {
        previous.stop();
    }
    let session = hls_handler.on_play(&url, start_pos);

    if let Ok(mut state) = conn.hls_state.lock() {
        state.session = Some(session);
        state.session_id = session_id;
        state.owner = Some(conn.close_handle.clone());
    }
    if let Some(previous_owner) = previous_owner
        && previous_owner.id() != conn.close_handle.id()
    {
        previous_owner.close();
    }
    None
}

/// `GET /playback-info` — iPhone polls for playback state.
pub(crate) fn handle_playback_info(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let state = conn.hls_state.lock().ok()?;
    if !request_matches_session(request, state.session_id.as_deref()) {
        return None;
    }
    let session = state.session.as_ref()?;

    let duration = session.duration() as f64;
    let position = session.position() as f64;
    let rate = session.rate() as f64;
    let ready = session.ready();

    let mut dict = plist::Dictionary::new();
    dict.insert("duration".into(), plist::Value::Real(duration));
    dict.insert("position".into(), plist::Value::Real(position));
    dict.insert("rate".into(), plist::Value::Real(rate));
    dict.insert("readyToPlay".into(), plist::Value::Integer((ready as i64).into()));
    dict.insert("playbackBufferEmpty".into(), plist::Value::Integer(0_i64.into()));
    dict.insert("playbackBufferFull".into(), plist::Value::Integer(1_i64.into()));
    dict.insert("playbackLikelyToKeepUp".into(), plist::Value::Integer(1_i64.into()));

    // loadedTimeRanges
    let mut loaded = plist::Dictionary::new();
    loaded.insert("start".into(), plist::Value::Real(position));
    loaded.insert("duration".into(), plist::Value::Real((duration - position).max(0.0)));
    dict.insert(
        "loadedTimeRanges".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(loaded)]),
    );

    // seekableTimeRanges
    let mut seekable = plist::Dictionary::new();
    seekable.insert("start".into(), plist::Value::Real(0.0));
    seekable.insert("duration".into(), plist::Value::Real(duration));
    dict.insert(
        "seekableTimeRanges".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(seekable)]),
    );

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(dict)).ok()?;
    response.add_header("Content-Type", "text/x-apple-plist+xml");
    Some(buf)
}

/// `POST /scrub?position=X` — seek to position.
pub(crate) fn handle_scrub(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let url = request.url()?;
    let pos = parse_query_float(url, "position")?;
    tracing::debug!(pos, "HLS scrub");
    if let Ok(mut state) = conn.hls_state.lock()
        && request_matches_session(request, state.session_id.as_deref())
        && let Some(session) = state.session.as_mut()
    {
        session.seek(pos);
    }
    None
}

/// `GET /scrub` — legacy AirPlay clients query duration and position as
/// `text/parameters` rather than using `/playback-info`.
pub(crate) fn handle_scrub_info(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let state = conn.hls_state.lock().ok()?;
    if !request_matches_session(request, state.session_id.as_deref()) {
        return None;
    }
    let session = state.session.as_ref()?;
    response.add_header("Content-Type", "text/parameters");
    Some(
        format!(
            "duration: {:.6}\r\nposition: {:.6}\r\n",
            session.duration().max(0.0),
            session.position().max(0.0)
        )
        .into_bytes(),
    )
}

/// `POST /rate?value=X` — set playback rate (0=pause, 1=play).
pub(crate) fn handle_rate(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    let url = request.url()?;
    let rate = parse_query_float(url, "value")?;
    tracing::debug!(rate, "HLS rate");
    if let Ok(mut state) = conn.hls_state.lock()
        && request_matches_session(request, state.session_id.as_deref())
        && let Some(session) = state.session.as_mut()
    {
        session.set_rate(rate);
    }
    None
}

/// `POST /stop` — stop HLS playback.
pub(crate) fn handle_stop(
    conn: &mut RaopConnection,
    request: &HttpRequest,
    _response: &mut HttpResponse,
) -> Option<Vec<u8>> {
    tracing::info!("HLS stop");
    if let Ok(mut state) = conn.hls_state.lock() {
        if !request_matches_session(request, state.session_id.as_deref()) {
            return None;
        }
        if let Some(session) = state.session.as_mut() {
            session.stop();
        }
        state.session = None;
        state.session_id = None;
        state.owner = None;
    }
    None
}

/// Reject an explicitly different AirPlay session while remaining compatible
/// with legacy clients that omit `X-Apple-Session-ID` on follow-up requests.
fn request_matches_session(request: &HttpRequest, current: Option<&str>) -> bool {
    match (current, request.header("X-Apple-Session-ID")) {
        (Some(current), Some(requested)) => current == requested,
        _ => true,
    }
}

/// Parse `?key=value` from a URL query string.
fn parse_query_float(url: &str, key: &str) -> Option<f32> {
    let query = url.split('?').nth(1)?;
    for param in query.split('&') {
        if let Some(val) = param.strip_prefix(key).and_then(|s| s.strip_prefix('=')) {
            return val.parse().ok();
        }
    }
    None
}

/// AirPlay senders use either a binary/XML plist or the legacy
/// `text/parameters` line format for `/play`. Support both forms.
fn parse_play_body(data: &[u8]) -> Option<(String, f32)> {
    if let Ok(plist_val) = plist::from_bytes::<plist::Value>(data)
        && let Some(dict) = plist_val.as_dictionary()
        && let Some(url) = dict.get("Content-Location").and_then(|value| value.as_string())
    {
        let start = dict.get("Start-Position").and_then(plist_number).unwrap_or(0.0) as f32;
        return Some((url.to_owned(), start));
    }

    let body = std::str::from_utf8(data).ok()?;
    let mut url = None;
    let mut start = 0.0_f32;
    for line in body.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-location" => url = Some(value.trim().to_owned()),
            "start-position" => {
                start = value.trim().parse::<f32>().unwrap_or(0.0);
            }
            _ => {}
        }
    }
    url.map(|url| (url, start))
}

fn plist_number(value: &plist::Value) -> Option<f64> {
    value
        .as_real()
        .or_else(|| value.as_signed_integer().map(|number| number as f64))
        .or_else(|| value.as_unsigned_integer().map(|number| number as f64))
        .or_else(|| value.as_string()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_float_basic() {
        assert_eq!(parse_query_float("/scrub?position=12.5", "position"), Some(12.5));
        assert_eq!(parse_query_float("/rate?value=1.0", "value"), Some(1.0));
        assert_eq!(parse_query_float("/rate?value=0.0", "value"), Some(0.0));
    }

    #[test]
    fn parse_query_float_missing() {
        assert_eq!(parse_query_float("/scrub", "position"), None);
        assert_eq!(parse_query_float("/scrub?other=1", "position"), None);
    }

    #[test]
    fn parse_query_float_multiple_params() {
        assert_eq!(parse_query_float("/x?a=1&position=2.75&b=2", "position"), Some(2.75));
    }

    #[test]
    fn parse_query_float_invalid() {
        assert_eq!(parse_query_float("/scrub?position=abc", "position"), None);
        assert_eq!(parse_query_float("/scrub?position=", "position"), None);
    }

    #[test]
    fn explicit_session_id_must_match_but_legacy_omission_is_allowed() {
        let mut matching = HttpRequest::new();
        matching
            .add_data(b"GET /playback-info HTTP/1.1\r\nX-Apple-Session-ID: current\r\n\r\n")
            .unwrap();
        assert!(request_matches_session(&matching, Some("current")));
        assert!(!request_matches_session(&matching, Some("other")));

        let mut legacy = HttpRequest::new();
        legacy.add_data(b"GET /playback-info HTTP/1.1\r\n\r\n").unwrap();
        assert!(request_matches_session(&legacy, Some("current")));
    }

    #[test]
    fn parse_play_body_accepts_legacy_text_parameters() {
        let (url, position) =
            parse_play_body(b"Content-Location: https://example.test/master.m3u8\r\nStart-Position: 12.5\r\n").unwrap();
        assert_eq!(url, "https://example.test/master.m3u8");
        assert_eq!(position, 12.5);
    }

    #[test]
    fn parse_play_body_accepts_binary_plist() {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "Content-Location".into(),
            plist::Value::String("https://example.test/video.mp4".into()),
        );
        dict.insert("Start-Position".into(), plist::Value::Real(3.25));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &plist::Value::Dictionary(dict)).unwrap();

        assert_eq!(
            parse_play_body(&body),
            Some(("https://example.test/video.mp4".into(), 3.25))
        );
    }
}
