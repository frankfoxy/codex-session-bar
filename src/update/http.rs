//! Native, bounded HTTPS streaming; no shell, account credentials or Git process.
use anyhow::{ensure, Context, Result};
use std::{
    ffi::c_void,
    io::Write,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR},
    Win32::Networking::WinHttp::*,
};

struct Handle(*mut c_void);
impl Handle {
    fn new(value: *mut c_void) -> Result<Self> {
        ensure!(
            !value.is_null(),
            "HTTPS initialization failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(value))
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub struct Response {
    pub status: u32,
    pub etag: String,
    pub bytes: u64,
}
pub fn get(
    url: &str,
    etag: &str,
    output: &mut impl Write,
    limit: u64,
    stop: &AtomicBool,
) -> Result<Response> {
    request(
        url,
        etag,
        output,
        limit,
        stop,
        &["api.github.com", "github.com"],
    )
}
/// Public announcement reads have a separate host allowlist from update assets.
pub fn get_notice(
    url: &str,
    etag: &str,
    output: &mut impl Write,
    limit: u64,
    stop: &AtomicBool,
) -> Result<Response> {
    request(
        url,
        etag,
        output,
        limit,
        stop,
        &["divin.cc", "codex-resets.com"],
    )
}
fn request(
    url: &str,
    etag: &str,
    output: &mut impl Write,
    limit: u64,
    stop: &AtomicBool,
    hosts: &[&str],
) -> Result<Response> {
    ensure!(!stop.load(Ordering::Relaxed), "Update cancelled");
    ensure!(
        url.len() < 4096 && !url.contains(['\r', '\n']),
        "Invalid update URL"
    );
    let encoded: Vec<u16> = url.encode_utf16().collect();
    unsafe {
        let mut parts = URL_COMPONENTS {
            dwStructSize: std::mem::size_of::<URL_COMPONENTS>() as u32,
            dwHostNameLength: u32::MAX,
            dwUrlPathLength: u32::MAX,
            dwExtraInfoLength: u32::MAX,
            ..Default::default()
        };
        WinHttpCrackUrl(&encoded, 0, &mut parts)?;
        ensure!(
            parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS && parts.nPort == 443,
            "Updates require HTTPS on port 443"
        );
        let host = String::from_utf16_lossy(std::slice::from_raw_parts(
            parts.lpszHostName.0,
            parts.dwHostNameLength as usize,
        ));
        ensure!(hosts.contains(&host.as_str()), "Unexpected HTTPS host");
        let mut path = String::from_utf16_lossy(std::slice::from_raw_parts(
            parts.lpszUrlPath.0,
            parts.dwUrlPathLength as usize,
        ));
        if parts.dwExtraInfoLength > 0 {
            path.push_str(&String::from_utf16_lossy(std::slice::from_raw_parts(
                parts.lpszExtraInfo.0,
                parts.dwExtraInfoLength as usize,
            )));
        }
        let session = Handle::new(WinHttpOpen(
            w!("CodexSessionBar/Update"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000)?;
        let connection = Handle::new(WinHttpConnect(
            session.0,
            PCWSTR(wide(&host).as_ptr()),
            parts.nPort,
            0,
        ))?;
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            PCWSTR(wide(&path).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        // GitHub assets redirect to its CDN. Keep certificate checks and reject
        // HTTPS-to-HTTP redirects; no credentials are sent on these public GETs.
        WinHttpSetOption(
            Some(request.0),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(&WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_ne_bytes()),
        )?;
        let mut headers =
            "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
                .to_owned();
        if !etag.is_empty() && etag.len() < 512 && !etag.contains(['\r', '\n']) {
            headers.push_str(&format!("If-None-Match: {etag}\r\n"));
        }
        let headers: Vec<u16> = headers.encode_utf16().collect();
        let began = Instant::now();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)
            .context("Cannot contact HTTPS endpoint")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut())?;
        let mut status = 0u32;
        let mut length = 4;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut length,
            ptr::null_mut(),
        )?;
        let mut tag = [0u16; 512];
        let mut tag_length = (tag.len() * 2) as u32;
        let etag = if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_ETAG,
            PCWSTR::null(),
            Some(tag.as_mut_ptr().cast()),
            &mut tag_length,
            ptr::null_mut(),
        )
        .is_ok()
        {
            String::from_utf16_lossy(&tag[..(tag_length as usize / 2).min(tag.len())])
                .trim_end_matches('\0')
                .to_owned()
        } else {
            String::new()
        };
        if status == 304 {
            return Ok(Response {
                status,
                etag,
                bytes: 0,
            });
        }
        ensure!(status == 200, "HTTPS endpoint returned HTTP {status}");
        let mut buffer = [0u8; 65536];
        let mut total = 0u64;
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "Update cancelled");
            ensure!(
                began.elapsed() < Duration::from_secs(120),
                "HTTPS request timed out"
            );
            let mut count = 0;
            WinHttpReadData(
                request.0,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut count,
            )?;
            if count == 0 {
                break;
            }
            total += count as u64;
            ensure!(total <= limit, "Update response exceeds its size limit");
            output.write_all(&buffer[..count as usize])?;
        }
        Ok(Response {
            status,
            etag,
            bytes: total,
        })
    }
}
