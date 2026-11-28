//! Standalone minimal Chrome/Edge Native Messaging Host (`com.genshinhelper.host`).
//! Console subsystem is REQUIRED for 4-byte little-endian stdio framing (`src/native_host.py`).

use gh_core::protocol::{MAX_NATIVE_MESSAGE_BYTES, NativeResponse};
#[cfg(windows)]
use gh_core::ELEVATED_TASK_NAME;
use gh_core::{DEFAULT_SYNC_PORT, HOST_VERSION};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const SCHTASKS_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_LAUNCH_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
struct HostEnv {
    port: u16,
    launch_timeout: Duration,
    #[allow(dead_code)]
    skip_schtasks: bool,
    server_exe_override: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    current_dir: Option<PathBuf>,
}

impl HostEnv {
    fn from_process(port: u16) -> Self {
        let skip_schtasks = std::env::var("GH_HOST_SKIP_SCHTASKS")
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        let server_exe_override = std::env::var_os("GH_HOST_SERVER_EXE")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        Self {
            port,
            launch_timeout: DEFAULT_LAUNCH_TIMEOUT,
            skip_schtasks,
            server_exe_override,
            current_exe: std::env::current_exe().ok(),
            current_dir: std::env::current_dir().ok(),
        }
    }
}

fn parse_port_from_args<I, S>(args: I) -> u16
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut port = DEFAULT_SYNC_PORT;
    let mut iter = args.into_iter().skip(1);
    while let Some(arg) = iter.next() {
        let s = arg.as_ref();
        if let Some(val) = s.strip_prefix("--port=") {
            if let Ok(p) = val.trim().parse::<u16>()
                && p >= 1
            {
                port = p;
            }
        } else if s == "--port"
            && let Some(next) = iter.next()
            && let Ok(p) = next.as_ref().trim().parse::<u16>()
            && p >= 1
        {
            port = p;
        }
    }
    port
}

fn read_native_frame<R: Read>(reader: &mut R) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 4];
    if reader.read_exact(&mut hdr).is_err() {
        return None;
    }
    let msg_len = u32::from_le_bytes(hdr) as usize;
    if msg_len == 0 || msg_len > MAX_NATIVE_MESSAGE_BYTES {
        return None;
    }
    let mut buf = vec![0u8; msg_len];
    if reader.read_exact(&mut buf).is_err() {
        return None;
    }
    Some(buf)
}

fn write_native_json<W: Write>(writer: &mut W, resp: &NativeResponse) -> io::Result<()> {
    let encoded = serde_json::to_vec(resp)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if encoded.len() > MAX_NATIVE_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native response exceeds max frame size",
        ));
    }
    let len_le = (encoded.len() as u32).to_le_bytes();
    writer.write_all(&len_le)?;
    writer.write_all(&encoded)?;
    writer.flush()
}

fn make_error_response(error: String) -> NativeResponse {
    NativeResponse {
        ok: false,
        host_version: HOST_VERSION.to_owned(),
        server_running: false,
        engine_active: None,
        version: None,
        started: None,
        mode: None,
        acknowledged: None,
        error: Some(error),
    }
}

fn http_json(
    port: u16,
    method: &str,
    path: &str,
    payload: Option<&Value>,
    timeout: Duration,
) -> Option<Value> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let body_bytes = match payload {
        Some(v) => Some(serde_json::to_vec(v).ok()?),
        None => None,
    };

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    )
    .into_bytes();
    if let Some(ref body) = body_bytes {
        req.extend_from_slice(b"Content-Type: application/json\r\n");
        req.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        req.extend_from_slice(body);
    } else {
        req.extend_from_slice(b"\r\n");
    }

    stream.write_all(&req).ok()?;
    let _ = stream.flush();

    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() >= MAX_HTTP_HEADER_BYTES {
            return None;
        }
        match stream.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    };

    let header_bytes = &buf[..header_end];
    let header_str = std::str::from_utf8(header_bytes).ok()?;
    let mut lines = header_str.split("\r\n");
    let status_line = lines.next()?;
    let status_code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    if !(200..300).contains(&status_code) {
        return None;
    }

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = val.parse::<usize>().ok();
            } else if key.eq_ignore_ascii_case("transfer-encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }

    let mut raw_body = buf[header_end + 4..].to_vec();
    let body = if is_chunked {
        while !is_chunked_complete(&raw_body) && raw_body.len() < MAX_HTTP_BODY_BYTES {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => raw_body.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        decode_chunked(&raw_body)?
    } else if let Some(len) = content_length {
        if len > MAX_HTTP_BODY_BYTES {
            return None;
        }
        while raw_body.len() < len {
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => raw_body.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        raw_body.truncate(len);
        raw_body
    } else {
        while raw_body.len() < MAX_HTTP_BODY_BYTES {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => raw_body.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        raw_body
    };

    if body.iter().all(|b| b.is_ascii_whitespace()) {
        Some(json!({}))
    } else {
        serde_json::from_slice::<Value>(&body).ok()
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_chunked_complete(buf: &[u8]) -> bool {
    decode_chunked(buf).is_some()
}

fn decode_chunked(buf: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let rel_crlf = buf[pos..].windows(2).position(|w| w == b"\r\n")?;
        let size_line = std::str::from_utf8(&buf[pos..pos + rel_crlf]).ok()?;
        let size_hex = size_line.split(';').next()?.trim();
        let chunk_len = usize::from_str_radix(size_hex, 16).ok()?;
        pos += rel_crlf + 2;
        if chunk_len == 0 {
            return Some(out);
        }
        if pos + chunk_len + 2 > buf.len() {
            return None;
        }
        out.extend_from_slice(&buf[pos..pos + chunk_len]);
        if &buf[pos + chunk_len..pos + chunk_len + 2] != b"\r\n" {
            return None;
        }
        pos += chunk_len + 2;
    }
}

fn launch_desktop_app(env: &HostEnv) -> bool {
    #[cfg(windows)]
    if !env.skip_schtasks {
        let mut cmd = Command::new("schtasks");
        cmd.args(["/run", "/tn", ELEVATED_TASK_NAME])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if let Ok(mut child) = cmd.spawn() {
            let deadline = Instant::now() + SCHTASKS_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            return true;
                        }
                        break;
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(ref override_exe) = env.server_exe_override {
        candidates.push(override_exe.clone());
    }
    let bin_name = if cfg!(windows) {
        "GenshinHelper.exe"
    } else {
        "GenshinHelper"
    };
    if let Some(ref exe) = env.current_exe
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(bin_name));
    }
    if let Some(ref cwd) = env.current_dir {
        candidates.push(cwd.join(bin_name));
    }

    for exe_path in candidates {
        if !exe_path.is_file() {
            continue;
        }
        if spawn_detached_server(&exe_path) {
            return true;
        }
    }

    false
}

fn spawn_detached_server(exe_path: &Path) -> bool {
    let mut cmd = Command::new(exe_path);
    cmd.arg("--minimized")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(parent) = exe_path.parent()
        && !parent.as_os_str().is_empty()
    {
        cmd.current_dir(parent);
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    cmd.spawn().is_ok()
}

fn handle_command(msg: &serde_json::Map<String, Value>, env: &HostEnv) -> NativeResponse {
    let cmd = msg
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match cmd.as_str() {
        "ping" => {
            if let Some(st) = http_json(
                env.port,
                "GET",
                "/status",
                None,
                Duration::from_millis(800),
            ) {
                let engine_active = st
                    .get("engine_active")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let version = st
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: true,
                    engine_active: Some(engine_active),
                    version,
                    started: None,
                    mode: None,
                    acknowledged: None,
                    error: None,
                }
            } else {
                NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: false,
                    engine_active: Some(false),
                    version: None,
                    started: None,
                    mode: None,
                    acknowledged: None,
                    error: None,
                }
            }
        }
        "start_server" => {
            if let Some(st) = http_json(
                env.port,
                "GET",
                "/status",
                None,
                Duration::from_millis(800),
            ) {
                let engine_active = st
                    .get("engine_active")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if !engine_active {
                    let wake_body = json!({ "active": true });
                    let _ = http_json(
                        env.port,
                        "POST",
                        "/set_engine_state",
                        Some(&wake_body),
                        Duration::from_millis(1500),
                    );
                    return NativeResponse {
                        ok: true,
                        host_version: HOST_VERSION.to_owned(),
                        server_running: true,
                        engine_active: Some(true),
                        version: None,
                        started: Some("woke_standby".to_owned()),
                        mode: None,
                        acknowledged: None,
                        error: None,
                    };
                }
                return NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: true,
                    engine_active: Some(true),
                    version: None,
                    started: Some("already_active".to_owned()),
                    mode: None,
                    acknowledged: None,
                    error: None,
                };
            }

            if !launch_desktop_app(env) {
                return make_error_response(
                    "Could not launch GenshinHelper executable or scheduled task.".to_owned(),
                );
            }

            let timeout = env.launch_timeout.max(Duration::from_millis(500));
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Some(st) = http_json(
                    env.port,
                    "GET",
                    "/status",
                    None,
                    Duration::from_millis(600),
                ) {
                    let engine_active = st
                        .get("engine_active")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if !engine_active {
                        let wake_body = json!({ "active": true });
                        let _ = http_json(
                            env.port,
                            "POST",
                            "/set_engine_state",
                            Some(&wake_body),
                            Duration::from_millis(1200),
                        );
                    }
                    return NativeResponse {
                        ok: true,
                        host_version: HOST_VERSION.to_owned(),
                        server_running: true,
                        engine_active: Some(true),
                        version: None,
                        started: Some("launched".to_owned()),
                        mode: None,
                        acknowledged: None,
                        error: None,
                    };
                }
                std::thread::sleep(Duration::from_millis(200));
            }

            make_error_response("Timed out waiting for GenshinHelper server on localhost.".to_owned())
        }
        "stop_server" => {
            let mode = msg
                .get("mode")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("standby")
                .to_ascii_lowercase();

            if mode == "shutdown" {
                let empty = json!({});
                let res = http_json(
                    env.port,
                    "POST",
                    "/shutdown",
                    Some(&empty),
                    Duration::from_millis(1200),
                );
                return NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: false,
                    engine_active: Some(false),
                    version: None,
                    started: None,
                    mode: Some("shutdown".to_owned()),
                    acknowledged: Some(res.is_some()),
                    error: None,
                };
            }

            let standby_body = json!({ "active": false });
            if let Some(res) = http_json(
                env.port,
                "POST",
                "/set_engine_state",
                Some(&standby_body),
                Duration::from_millis(1200),
            ) {
                let engine_active = res
                    .get("engine_active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: true,
                    engine_active: Some(engine_active),
                    version: None,
                    started: None,
                    mode: Some("standby".to_owned()),
                    acknowledged: None,
                    error: None,
                }
            } else {
                NativeResponse {
                    ok: true,
                    host_version: HOST_VERSION.to_owned(),
                    server_running: false,
                    engine_active: Some(false),
                    version: None,
                    started: None,
                    mode: Some("standby".to_owned()),
                    acknowledged: None,
                    error: None,
                }
            }
        }
        _ => NativeResponse {
            ok: false,
            host_version: HOST_VERSION.to_owned(),
            server_running: false,
            engine_active: None,
            version: None,
            started: None,
            mode: None,
            acknowledged: None,
            error: Some(format!("Unknown command: {cmd}")),
        },
    }
}

fn handle_message_bytes(raw: &[u8], env: &HostEnv) -> NativeResponse {
    let val: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => return make_error_response(e.to_string()),
    };
    let Some(obj) = val.as_object() else {
        return make_error_response("Expected JSON object payload".to_owned());
    };
    handle_command(obj, env)
}

fn main() {
    let port = parse_port_from_args(std::env::args());
    let env = HostEnv::from_process(port);

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    while let Some(frame) = read_native_frame(&mut reader) {
        let resp = handle_message_bytes(&frame, &env);
        if write_native_json(&mut writer, &resp).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn frame_bytes(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn reserve_unused_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn cli_port_parsing_tolerates_chrome_origin_and_unknown_flags() {
        assert_eq!(
            parse_port_from_args(["GenshinHelperHost.exe"]),
            DEFAULT_SYNC_PORT
        );
        assert_eq!(
            parse_port_from_args([
                "GenshinHelperHost.exe",
                "--port",
                "9123",
                "chrome-extension://nfkijdoblmphmdljfogooajhbldkeojh/",
                "--parent-window=12345"
            ]),
            9123
        );
        assert_eq!(
            parse_port_from_args([
                "GenshinHelperHost.exe",
                "--unknown",
                "--port=9456",
                "chrome-extension://abc/"
            ]),
            9456
        );
        // Port 0 or non-numeric ignored
        assert_eq!(
            parse_port_from_args(["GenshinHelperHost.exe", "--port", "0", "--port=bad"]),
            DEFAULT_SYNC_PORT
        );
    }

    #[test]
    fn stdio_framing_boundaries_and_utf8_roundtrip() {
        // Immediate EOF
        assert!(read_native_frame(&mut Cursor::new(Vec::<u8>::new())).is_none());
        // Zero-length frame
        assert!(read_native_frame(&mut Cursor::new(0u32.to_le_bytes().to_vec())).is_none());
        // Oversized frame
        let mut over = (MAX_NATIVE_MESSAGE_BYTES as u32 + 1).to_le_bytes().to_vec();
        over.extend_from_slice(b"{}");
        assert!(read_native_frame(&mut Cursor::new(over)).is_none());
        // Truncated header
        assert!(read_native_frame(&mut Cursor::new(vec![0x05, 0x00])).is_none());
        // Truncated body
        let mut trunc_body = 10u32.to_le_bytes().to_vec();
        trunc_body.extend_from_slice(b"abc");
        assert!(read_native_frame(&mut Cursor::new(trunc_body)).is_none());

        // Valid Vietnamese UTF-8 roundtrip
        let payload = r#"{"cmd":"ping","note":"Kiểm tra tiếng Việt UTF-8"}"#.as_bytes();
        let framed = frame_bytes(payload);
        let decoded = read_native_frame(&mut Cursor::new(framed)).expect("valid frame");
        assert_eq!(decoded, payload);

        let resp = NativeResponse {
            ok: true,
            host_version: HOST_VERSION.to_owned(),
            server_running: false,
            engine_active: Some(false),
            version: None,
            started: None,
            mode: None,
            acknowledged: None,
            error: None,
        };
        let mut out = Vec::new();
        write_native_json(&mut out, &resp).unwrap();
        assert!(!out.starts_with(b"\xEF\xBB\xBF"));
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(len, out.len() - 4);
        let parsed: Value = serde_json::from_slice(&out[4..]).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["host_version"], HOST_VERSION);
    }

    #[test]
    fn invalid_json_list_and_unknown_command_handling() {
        let env = HostEnv {
            port: reserve_unused_port(),
            launch_timeout: Duration::from_millis(600),
            skip_schtasks: true,
            server_exe_override: None,
            current_exe: None,
            current_dir: None,
        };

        let r_bad = handle_message_bytes(b"{{{not_json", &env);
        assert!(!r_bad.ok);
        assert_eq!(r_bad.host_version, HOST_VERSION);
        assert!(r_bad.error.is_some());

        let r_list = handle_message_bytes(b"[]", &env);
        assert!(!r_list.ok);
        assert_eq!(r_list.host_version, HOST_VERSION);
        assert!(r_list.error.is_some());

        let r_unknown = handle_message_bytes(br#"{"cmd":"  Unknown_Contract_Cmd  "}"#, &env);
        assert!(!r_unknown.ok);
        assert_eq!(r_unknown.host_version, HOST_VERSION);
        assert_eq!(
            r_unknown.error.as_deref(),
            Some("Unknown command: unknown_contract_cmd")
        );
    }

    #[test]
    fn offline_ping_stop_and_failed_cold_launch() {
        let env = HostEnv {
            port: reserve_unused_port(),
            launch_timeout: Duration::from_millis(600),
            skip_schtasks: true,
            server_exe_override: None,
            current_exe: None,
            current_dir: None,
        };

        let r_ping = handle_message_bytes(br#"{"cmd":"ping"}"#, &env);
        assert!(r_ping.ok);
        assert!(!r_ping.server_running);
        assert_eq!(r_ping.engine_active, Some(false));

        let r_stop_standby = handle_message_bytes(br#"{"cmd":"stop_server","mode":"standby"}"#, &env);
        assert!(r_stop_standby.ok);
        assert!(!r_stop_standby.server_running);
        assert_eq!(r_stop_standby.mode.as_deref(), Some("standby"));

        let r_stop_shut = handle_message_bytes(br#"{"cmd":"stop_server","mode":"shutdown"}"#, &env);
        assert!(r_stop_shut.ok);
        assert!(!r_stop_shut.server_running);
        assert_eq!(r_stop_shut.mode.as_deref(), Some("shutdown"));
        assert_eq!(r_stop_shut.acknowledged, Some(false));

        let r_start = handle_message_bytes(br#"{"cmd":"start_server"}"#, &env);
        assert!(!r_start.ok);
        assert!(!r_start.server_running);
        assert!(
            r_start
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Could not launch"))
        );
    }

    #[test]
    fn online_mock_server_ping_wake_standby_and_shutdown_plus_chunked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let engine_active = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let req_count = Arc::new(AtomicUsize::new(0));

        let ea = Arc::clone(&engine_active);
        let sf = Arc::clone(&stop_flag);
        let rc = Arc::clone(&req_count);

        let srv_handle = std::thread::spawn(move || {
            while !sf.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                rc.fetch_add(1, Ordering::SeqCst);
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while let Ok(n) = stream.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(hdr_end) = find_header_end(&buf) {
                        let hdr = String::from_utf8_lossy(&buf[..hdr_end]);
                        let cl = hdr
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if buf.len() >= hdr_end + 4 + cl {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf);
                let first_line = req.lines().next().unwrap_or("");
                let resp_body = if first_line.starts_with("GET /status") {
                    let active = ea.load(Ordering::SeqCst);
                    format!(r#"{{"version":"5.8.0","engine_active":{active}}}"#)
                } else if first_line.starts_with("POST /set_engine_state") {
                    let active = req.contains("true");
                    ea.store(active, Ordering::SeqCst);
                    format!(r#"{{"ok":true,"engine_active":{active}}}"#)
                } else if first_line.starts_with("POST /shutdown") {
                    sf.store(true, Ordering::SeqCst);
                    r#"{"ok":true,"shutting_down":true}"#.to_owned()
                } else {
                    "{}".to_owned()
                };

                // Alternate Content-Length and Transfer-Encoding: chunked to test both decoders
                let raw_resp = if rc.load(Ordering::SeqCst).is_multiple_of(2) {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                        resp_body.len(),
                        resp_body
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    )
                };
                let _ = stream.write_all(raw_resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let env = HostEnv {
            port,
            launch_timeout: Duration::from_millis(800),
            skip_schtasks: true,
            server_exe_override: None,
            current_exe: None,
            current_dir: None,
        };

        let r_ping = handle_message_bytes(br#"{"cmd":"ping"}"#, &env);
        assert!(r_ping.ok);
        assert!(r_ping.server_running);
        assert_eq!(r_ping.engine_active, Some(false));
        assert_eq!(r_ping.version.as_deref(), Some("5.8.0"));

        let r_wake = handle_message_bytes(br#"{"cmd":"start_server"}"#, &env);
        assert!(r_wake.ok);
        assert_eq!(r_wake.started.as_deref(), Some("woke_standby"));
        assert_eq!(r_wake.engine_active, Some(true));

        let r_active = handle_message_bytes(br#"{"cmd":"start_server"}"#, &env);
        assert!(r_active.ok);
        assert_eq!(r_active.started.as_deref(), Some("already_active"));

        let r_standby = handle_message_bytes(br#"{"cmd":"stop_server","mode":"standby"}"#, &env);
        assert!(r_standby.ok);
        assert!(r_standby.server_running);
        assert_eq!(r_standby.engine_active, Some(false));
        assert_eq!(r_standby.mode.as_deref(), Some("standby"));

        let r_shut = handle_message_bytes(br#"{"cmd":"stop_server","mode":"shutdown"}"#, &env);
        assert!(r_shut.ok);
        assert!(!r_shut.server_running);
        assert_eq!(r_shut.mode.as_deref(), Some("shutdown"));
        assert_eq!(r_shut.acknowledged, Some(true));

        let _ = srv_handle.join();
    }
}
