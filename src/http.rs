//! Minimal HTTPS GET over WinHTTP.
//!
//! WinHTTP ships with Windows, uses the system TLS stack and certificate store,
//! and honours the system proxy -- so the app bundles no TLS library at all.
//! One session is shared by all threads; WinHTTP pools connections inside it,
//! which gives the keep-alive the Python version got from requests.Session.

use std::ffi::c_void;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

#[derive(Debug)]
pub enum HttpError {
    /// No connection, DNS failure, TLS failure or timeout.
    Transport(String),
}

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

pub struct Client {
    session: *mut c_void,
    timeouts: Timeouts,
}

/// WinHTTP timeouts in milliseconds. Reaching the server should be quick, so
/// a dead network fails fast; the answer itself may take longer, because a
/// slow but working server is not an outage.
#[derive(Clone, Copy)]
pub struct Timeouts {
    pub resolve_ms: i32,
    pub connect_ms: i32,
    pub send_ms: i32,
    pub receive_ms: i32,
}

impl Timeouts {
    fn apply(&self, handle: *mut c_void) {
        unsafe {
            let _ = WinHttpSetTimeouts(handle, self.resolve_ms, self.connect_ms, self.send_ms, self.receive_ms);
        }
    }
}

// WinHTTP session handles may be used from any thread.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

impl Client {
    pub fn new(user_agent: &str, timeouts: Timeouts) -> Self {
        let agent = HSTRING::from(user_agent);
        let session = unsafe {
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        };
        if !session.is_null() {
            timeouts.apply(session);
        }
        Client { session, timeouts }
    }

    /// GET https://{host}{path}?{query}. Any HTTP status is a Response; only
    /// failures to get one at all are errors.
    pub fn get(&self, host: &str, path: &str, query: &[(&str, String)]) -> Result<Response, HttpError> {
        let started = std::time::Instant::now();
        let shown: Vec<String> = query.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
        log!("http: GET {host}{path} {}", shown.join(" "));
        let result = self.get_inner(host, path, query);
        let ms = started.elapsed().as_millis();
        match &result {
            Ok(r) => log!("http: {path} -> HTTP {} in {ms} ms, {} bytes", r.status, r.body.len()),
            Err(HttpError::Transport(e)) => log!("http: {path} -> FAILED after {ms} ms: {e}"),
        }
        result
    }

    fn get_inner(&self, host: &str, path: &str, query: &[(&str, String)]) -> Result<Response, HttpError> {
        if self.session.is_null() {
            return Err(HttpError::Transport("WinHttpOpen failed".into()));
        }
        let mut object = String::from(path);
        if !query.is_empty() {
            object.push('?');
            for (i, (k, v)) in query.iter().enumerate() {
                if i > 0 {
                    object.push('&');
                }
                object.push_str(&encode(k));
                object.push('=');
                object.push_str(&encode(v));
            }
        }

        unsafe {
            let host_w = HSTRING::from(host);
            let connect = WinHttpConnect(self.session, &host_w, INTERNET_DEFAULT_HTTPS_PORT, 0);
            if connect.is_null() {
                return Err(last_error("connect"));
            }
            let _close_connect = Handle(connect);

            let object_w = HSTRING::from(object.as_str());
            let request = WinHttpOpenRequest(
                connect,
                w!("GET"),
                &object_w,
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            );
            if request.is_null() {
                return Err(last_error("open request"));
            }
            let _close_request = Handle(request);
            self.timeouts.apply(request);

            WinHttpSendRequest(request, None, None, 0, 0, 0).map_err(|e| HttpError::Transport(describe(&e)))?;
            WinHttpReceiveResponse(request, std::ptr::null_mut()).map_err(|e| HttpError::Transport(describe(&e)))?;

            let mut status: u32 = 0;
            let mut size = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut size,
                std::ptr::null_mut(),
            )
            .map_err(|e| HttpError::Transport(describe(&e)))?;

            let mut body = Vec::new();
            let mut chunk = vec![0u8; 16 * 1024];
            loop {
                let mut read: u32 = 0;
                WinHttpReadData(request, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32, &mut read)
                    .map_err(|e| HttpError::Transport(describe(&e)))?;
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read as usize]);
            }
            Ok(Response { status, body })
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.session);
            }
        }
    }
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn last_error(what: &str) -> HttpError {
    HttpError::Transport(format!("{what}: {}", describe(&windows::core::Error::from_win32())))
}

/// Error text plus the WinHTTP error number, e.g. 12002 = timed out,
/// 12007 = name not resolved, 12029 = cannot connect, 12175 = TLS failure.
fn describe(e: &windows::core::Error) -> String {
    let code = e.code().0 as u32;
    let winhttp = if code & 0xFFFF_0000 == 0x8007_0000 { code & 0xFFFF } else { code };
    format!("{} (error {winhttp})", e.message().trim())
}

/// Percent-encode a query component (RFC 3986 unreserved characters pass).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
