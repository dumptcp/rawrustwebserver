//! rawstatic: dumptcp/rustwebserver's behaviour on a purpose-built
//! HTTP/1.1 engine instead of a framework.
//!
//! * one current-thread Tokio runtime per core, each with its own SO_REUSEPORT
//!   listener (the layout of HttpArena's fastest Rust engine entry);
//! * requests parsed with `httparse`, the fuzzed parser hyper itself uses;
//! * every 200/304/404 response head is built once per file and per encoding, and
//!   rebuilt when the background recompression swaps in smaller bodies, so a request
//!   only appends the Date line;
//! * file bodies go out with writev straight from the shared `Bytes`, no per-connection copy.
//! * file bodies and their pre-compressed copies stay in RAM only while they fit in
//!   `CACHE_MEMORY` (default: a quarter of usable memory). Anything past that is
//!   streamed from disk, so a site larger than RAM still starts.
//!
//! Anything with a request body (Content-Length > 0 or Transfer-Encoding) is answered
//! and the connection closed, so a body can never be misread as the next request.

// ================================================================ settings
// Version: cache-1-minute (HTML cached for 1 minute, other files for a year).
// This block is the only difference between the eight versions of this server; it
// matches what the actix version in the same folder of dumptcp/rustwebserver sends.

/// Keep a Brotli copy (strongest level) of every text file and send it to requests that
/// accept Brotli. Cloudflare always asks for Brotli and decompresses it for visitors
/// that cannot read it.
const COMPRESSION: bool = true;
/// Send an ETag with every file and answer 304 Not Modified while it still matches.
const ETAGS: bool = true;
/// Cache-Control for .html / .htm files, and for every other file.
const HTML_CACHE_CONTROL: &str = "public, max-age=60, no-transform";
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable, no-transform";
/// Add `no-transform` only to file types that are already compressed (images, fonts,
/// video, ...), so Cloudflare can still compress text files itself.
const NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES: bool = false;
/// Cache-Control for redirects, and for 404 / 405 / 416 responses.
const REDIRECT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable, no-transform";
const ERROR_CACHE_CONTROL: &str = "no-store";

use std::{
    cell::RefCell,
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
    io::Write,
    io::{self, IoSlice, Read},
    mem::MaybeUninit,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use http::StatusCode;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpStream,
    sync::watch,
    time::timeout,
};

static KEEP_ALIVE_SECS: AtomicU64 = AtomicU64::new(120);
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_IN_MEMORY: usize = 2 * 1024 * 1024;
/// An unfinished request head this long gets 431 (actix's limit, MAX_BUFFER_SIZE).
const MAX_HEAD: usize = 128 * 1024;
const MAX_HEADERS: usize = 96; // actix's limit; more gets 431
/// While a request is in progress, spare buffer capacity above this is dropped
/// so one large header cannot pin that memory for the rest of the connection.
/// An idle connection drops both buffers to zero instead.
const CONN_BUF_KEEP: usize = 8 * 1024;
/// Kernel cap on unsent bytes per socket (`TCP_NOTSENT_LOWAT`). A stalled reader
/// then cannot pin a large kernel send buffer. Linux only; other systems have
/// no equivalent that this server sets.
#[cfg(target_os = "linux")]
const NOTSENT_LOWAT: u32 = 16 * 1024;
/// Header-only replies (304, HEAD) flush once this much is pending, so a pipeline
/// cannot grow the connection buffer without bound. File bodies are not copied here.
const FLUSH_AT: usize = 8 * 1024;
/// Close a keep-alive connection after this many responses. A client that never
/// goes idle would otherwise hold its slot until the process exits.
const MAX_KEEPALIVE_REQUESTS: usize = 100_000;
/// How long a shutdown waits for in-flight responses (actix's default shutdown_timeout).
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Freed compression memory is handed back to the OS after this much is replaced.
const RETURN_MEMORY_EVERY: usize = 16 * 1024 * 1024;

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static OPEN_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
/// Highest open-connection count since the last `malloc_trim`.
static PEAK_SINCE_TRIM: AtomicUsize = AtomicUsize::new(0);
/// Unfinished-header bytes currently held beyond the normal per-connection buffers.
static HEAD_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Global cap for `HEAD_BYTES`. Over it, the connection gets 431 and is closed.
static HEAD_BUDGET: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Memory this process can use: the smaller of physical RAM and this process's
/// cgroup limit. RAM is `MemTotal` on Linux, `sysctl` on macOS and the BSDs,
/// and `GlobalMemoryStatusEx` on Windows. The cgroup limit is v2 `memory.max`
/// (this cgroup and its parents), or, when that file is absent, v1
/// `memory.limit_in_bytes`. A v1 "unlimited" sentinel (a value near 2^63) is
/// ignored. When RAM cannot be read, fall back to 1 GiB and no cgroup cap.
fn usable_memory() -> usize {
    mem_total().unwrap_or(1 << 30).min(cgroup_memory_limit())
}

fn mem_total() -> Option<usize> {
    platform_mem_total()
}

#[cfg(target_os = "linux")]
fn platform_mem_total() -> Option<usize> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd"
))]
fn platform_mem_total() -> Option<usize> {
    // macOS reports `hw.memsize`; the BSDs report `hw.physmem`. Both are bytes.
    sysctl_usize("hw.memsize").or_else(|| sysctl_usize("hw.physmem"))
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd"
))]
fn sysctl_usize(name: &str) -> Option<usize> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut mem: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut mem as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || mem == 0 {
        return None;
    }
    Some(usize::try_from(mem).unwrap_or(usize::MAX))
}

#[cfg(windows)]
fn platform_mem_total() -> Option<usize> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(lpbuffer: *mut MemoryStatusEx) -> i32;
    }
    unsafe {
        let mut status = std::mem::zeroed::<MemoryStatusEx>();
        status.length = std::mem::size_of::<MemoryStatusEx>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 || status.total_phys == 0 {
            return None;
        }
        Some(usize::try_from(status.total_phys).unwrap_or(usize::MAX))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "windows"
)))]
fn platform_mem_total() -> Option<usize> {
    None
}

/// Smallest numeric `memory.max` from this process's cgroup up through its
/// parents (`max` means unlimited). If that path cannot be read, the cgroup
/// root file, then the v1 limit, then unlimited.
fn cgroup_memory_limit() -> usize {
    self_cgroup_memory_limit()
        .or_else(|| memory_max_file(Path::new("/sys/fs/cgroup/memory.max")))
        .or_else(cgroup_v1_memory_limit)
        .unwrap_or(usize::MAX)
}

/// Smallest `memory.max` for the cgroup named by the `0::/path` line of `/proc/self/cgroup`.
fn self_cgroup_memory_limit() -> Option<usize> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    let mut dir = PathBuf::from("/sys/fs/cgroup");
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return None;
        }
        dir.push(part);
    }
    let root = Path::new("/sys/fs/cgroup");
    let mut best = usize::MAX;
    let mut saw = false;
    loop {
        if let Some(limit) = memory_max_file(&dir.join("memory.max")) {
            saw = true;
            best = best.min(limit);
        }
        if dir.as_path() == root {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    saw.then_some(best)
}

fn memory_max_file(path: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text == "max" {
        Some(usize::MAX)
    } else {
        let n: u64 = text.parse().ok()?;
        Some(finite_cgroup_limit(n).unwrap_or(usize::MAX))
    }
}

/// v1 `memory.limit_in_bytes`, used when cgroup v2 `memory.max` is not present.
/// The huge sentinel v1 writes for "unlimited" (near 2^63) is ignored.
fn cgroup_v1_memory_limit() -> Option<usize> {
    let text = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes").ok()?;
    let n: u64 = text.trim().parse().ok()?;
    finite_cgroup_limit(n)
}

/// `None` for cgroup v1's unlimited sentinel. Real limits are far below 2^62.
fn finite_cgroup_limit(n: u64) -> Option<usize> {
    if n >= 1 << 62 {
        None
    } else {
        Some(usize::try_from(n).unwrap_or(usize::MAX))
    }
}

/// Bytes of one connection's unfinished request head that sit past the normal buffers.
struct HeadCharge(usize);
impl HeadCharge {
    /// Count `want` bytes against the global budget. False leaves the charge unchanged.
    fn set(&mut self, want: usize) -> bool {
        if want > self.0 {
            let extra = want - self.0;
            let now = HEAD_BYTES.fetch_add(extra, Ordering::Relaxed) + extra;
            if now > HEAD_BUDGET.load(Ordering::Relaxed) {
                HEAD_BYTES.fetch_sub(extra, Ordering::Relaxed);
                return false;
            }
        } else {
            HEAD_BYTES.fetch_sub(self.0 - want, Ordering::Relaxed);
        }
        self.0 = want;
        true
    }
}
impl Drop for HeadCharge {
    fn drop(&mut self) {
        HEAD_BYTES.fetch_sub(self.0, Ordering::Relaxed);
    }
}

struct Counted;
impl Counted {
    /// `None` means no cap. The count still tracks open connections for shutdown
    /// and for returning freed memory to the system.
    fn try_new(max: Option<usize>) -> Option<Self> {
        loop {
            let n = OPEN_CONNECTIONS.load(Ordering::SeqCst);
            if max.is_some_and(|max| n >= max) {
                return None;
            }
            let Some(next) = n.checked_add(1) else {
                return None;
            };
            if OPEN_CONNECTIONS
                .compare_exchange(n, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                PEAK_SINCE_TRIM.fetch_max(next, Ordering::Relaxed);
                return Some(Counted);
            }
        }
    }
}
impl Drop for Counted {
    fn drop(&mut self) {
        OPEN_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

const SECURITY: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("x-frame-options", "DENY"),
    (
        "strict-transport-security",
        "max-age=31536000; includeSubDomains; preload",
    ),
    ("content-security-policy", CONTENT_SECURITY_POLICY_VALUE),
    ("permissions-policy", PERMISSIONS_POLICY_VALUE),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
];

// ---------------------------------------------------------------- from rustwebserver main.rs

const BROTLI_FAST: u32 = 6;
const BROTLI_BEST: u32 = 11;
const BROTLI_MIN_WINDOW: u32 = 16;
const BROTLI_MAX_WINDOW: u32 = 22;
const ACCEPT_BR: usize = 1;
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'none'; \
    img-src 'self' data: blob:; \
    media-src 'self' data: blob:; \
    style-src 'self' 'unsafe-inline'; \
    script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; \
    worker-src 'self' blob:; \
    font-src 'self'; \
    connect-src 'self'; \
    object-src 'none'; \
    upgrade-insecure-requests; \
    require-trusted-types-for 'script'; \
    trusted-types 'none'";
const PERMISSIONS_POLICY_VALUE: &str = "accelerometer=(), autoplay=(), camera=(), \
    display-capture=(), encrypted-media=(), fullscreen=(self), \
    geolocation=(), gyroscope=(), magnetometer=(), microphone=(), \
    midi=(), payment=(), picture-in-picture=(self), publickey-credentials-get=(), \
    screen-wake-lock=(), sync-xhr=(), usb=(), xr-spatial-tracking=()";
const REDIRECT_KEEP: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b',')
    .remove(b':')
    .remove(b';')
    .remove(b'=')
    .remove(b'@');

enum ByteRange {
    Ignore,
    Part(usize, usize),
    Unsatisfiable,
}

fn fnv1a(data: &[u8]) -> u64 {
    fnv1a_acc(0xcbf2_9ce4_8422_2325, data)
}

fn fnv1a_acc(mut h: u64, data: &[u8]) -> u64 {
    for b in data {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hash_file(path: &Path) -> std::io::Result<(u64, usize)> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 64 * 1024];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut size = 0usize;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size.saturating_add(n);
        h = fnv1a_acc(h, &buf[..n]);
    }
    Ok((h, size))
}

fn smaller(compressed: Option<Vec<u8>>, than: usize) -> Option<Bytes> {
    compressed
        .filter(|c| !c.is_empty() && c.len() < than)
        .map(|c| Bytes::from(c.into_boxed_slice()))
}

fn content_type(rel: &str) -> String {
    let ext = Path::new(rel)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let fixed = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "webmanifest" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        "yaml" | "yml" => "text/x-yaml; charset=utf-8",
        "toml" => "text/x-toml; charset=utf-8",
        "ini" | "conf" | "cfg" | "log" => "text/plain; charset=utf-8",
        "xslt" | "dtd" => "text/xml; charset=utf-8",
        "vcf" => "text/x-vcard; charset=utf-8",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tgz" => "application/x-compressed",
        "rar" => "application/x-rar-compressed",

        "ico" => "image/x-icon",
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "jxl" => "image/jxl",
        "heic" => "image/heic",

        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",

        "mp4" => "video/mp4",
        "m4v" => "video/x-m4v",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "3gp" => "video/3gpp",
        "ts" => "video/mp2t",
        "m3u8" => "application/vnd.apple.mpegurl",
        "mpd" => "application/dash+xml",

        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "weba" => "audio/webm",
        "mid" | "midi" => "audio/mid",
        "aif" | "aiff" => "audio/aiff",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_owned();
    }

    match mime_guess::from_ext(&ext).first_raw() {
        Some(m) if m.starts_with("text/") => format!("{m}; charset=utf-8"),
        Some(m) => m.to_owned(),
        None => "application/octet-stream".to_owned(),
    }
}

fn compressible(ctype: &str) -> bool {
    ctype.starts_with("text/")
        || [
            "javascript",
            "json",
            "svg",
            "xml",
            "x-icon",
            "font/ttf",
            "font/otf",
            "application/wasm",
        ]
        .iter()
        .any(|s| ctype.contains(s))
}

fn brotli_of(b: &[u8], quality: u32, window: u32) -> Option<Vec<u8>> {
    let mut w =
        brotli::CompressorWriter::new(Vec::with_capacity(b.len() / 2), 4096, quality, window);
    w.write_all(b).ok()?;
    Some(w.into_inner())
}

fn brotli_window(size: usize) -> u32 {
    let mut lgwin = BROTLI_MIN_WINDOW;
    while lgwin < BROTLI_MAX_WINDOW && (1usize << lgwin) - 16 < size {
        lgwin += 1;
    }
    lgwin
}

fn parse_range(header: &str, size: usize) -> ByteRange {
    const UNIT: &str = "bytes=";
    let unit_ok = header
        .get(..UNIT.len())
        .is_some_and(|u| u.eq_ignore_ascii_case(UNIT));
    if size == 0 || !unit_ok {
        return ByteRange::Ignore;
    }
    let spec = header[UNIT.len()..].trim();
    if spec.contains(',') {
        return ByteRange::Ignore;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Ignore;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        return match parse_pos(last) {
            None => ByteRange::Ignore,
            Some(0) => ByteRange::Unsatisfiable,
            Some(n) => ByteRange::Part(size - n.min(size), size - 1),
        };
    }

    let Some(start) = parse_pos(first) else {
        return ByteRange::Ignore;
    };
    let mut end = size - 1;
    if !last.is_empty() {
        match parse_pos(last) {
            Some(e) if e >= start => end = end.min(e),
            _ => return ByteRange::Ignore,
        }
    }
    if start >= size {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Part(start, end)
}

fn parse_pos(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for d in s.bytes() {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n.saturating_mul(10).saturating_add(usize::from(d - b'0'));
    }
    Some(n)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// `needle` must be lowercase.
fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn redirect_target(dir: &str) -> String {
    let encoded = utf8_percent_encode(dir, REDIRECT_KEEP).to_string();
    format!("/{}", encoded.trim_start_matches('/'))
}

fn query_suffix(q: &str) -> String {
    if q.is_empty() || !q.bytes().all(|b| b.is_ascii_graphic()) {
        return String::new();
    }
    format!("?{q}")
}

fn env_str(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn env_usize(key: &str, fallback: usize) -> usize {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or_else(|e| {
            eprintln!("bad {key}: {e}");
            std::process::exit(1);
        }),
        _ => fallback,
    }
}

/// `MAX_CONNECTIONS`: unset or 0 means no cap. Any other number is the cap.
fn env_max_connections() -> Option<usize> {
    match std::env::var("MAX_CONNECTIONS") {
        Ok(v) if !v.is_empty() => {
            let n: usize = v.parse().unwrap_or_else(|e| {
                eprintln!("bad MAX_CONNECTIONS: {e}");
                std::process::exit(1);
            });
            if n == 0 {
                None
            } else {
                Some(n)
            }
        }
        _ => None,
    }
}

fn max_connections_label(max: Option<usize>) -> String {
    match max {
        Some(n) => n.to_string(),
        None => "unlimited".to_owned(),
    }
}

fn env_bytes(key: &str, fallback: usize) -> usize {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => parse_byte_size(&v).unwrap_or_else(|e| {
            eprintln!("bad {key}: {e}");
            std::process::exit(1);
        }),
        _ => fallback,
    }
}

/// A byte count: a decimal number, or the same with a 1024-based suffix
/// (`K`, `M`, `G`, `T`). A trailing `B` or `iB` is accepted (`512M`, `512MB`, `2G`).
fn parse_byte_size(raw: &str) -> Result<usize, String> {
    let s = raw.trim();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(format!(
            "expected a number of bytes such as 512M or 2G, got {raw:?}"
        ));
    }
    let n: u64 = s[..digits]
        .parse()
        .map_err(|_| format!("expected a number of bytes such as 512M or 2G, got {raw:?}"))?;
    let suffix = s[digits..].trim();
    let mul: u64 = match suffix {
        "" | "B" | "b" => 1,
        "K" | "k" | "KB" | "kb" | "KiB" | "kib" => 1024,
        "M" | "m" | "MB" | "mb" | "MiB" | "mib" => 1024 * 1024,
        "G" | "g" | "GB" | "gb" | "GiB" | "gib" => 1024 * 1024 * 1024,
        "T" | "t" | "TB" | "tb" | "TiB" | "tib" => 1024 * 1024 * 1024 * 1024,
        _ => {
            return Err(format!(
                "unknown size suffix {suffix:?} in {raw:?} (use 512M, 2G, or a plain byte count)"
            ));
        }
    };
    let bytes = n
        .checked_mul(mul)
        .ok_or_else(|| format!("{raw:?} overflows"))?;
    usize::try_from(bytes).map_err(|_| format!("{raw:?} overflows"))
}

fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{n} bytes ({} MiB)", n / MIB)
    } else if n >= KIB {
        format!("{n} bytes ({} KiB)", n / KIB)
    } else {
        format!("{n} bytes")
    }
}

// ---------------------------------------------------------------- prebuilt files

#[derive(Clone)]
struct Variant {
    body: Bytes,
    enc: Option<&'static str>,
    /// Everything up to and including "date: " for a 200 (or 404 for 404.html).
    head: Bytes,
}
/// [original, Brotli]: Cloudflare always asks for `br, gzip` and decompresses Brotli
/// itself for clients without it, so no gzip copy is kept.
type PickTable = [Variant; 2];

struct File {
    ctype: String,
    cc: String,
    etag: String,
    body: Bytes,
    disk_path: Option<PathBuf>,
    size: usize,
    pick: ArcSwap<PickTable>,
    ranges: bool,
    /// Prebuilt 304 head up to "date: ".
    not_modified: Bytes,
    is_404_page: bool,
}

/// FxHash: a few multiplies per lookup instead of SipHash's rounds. Every key is a
/// path from `www`, chosen by the site owner, so there is no hash flooding to defend
/// against; requests only look keys up.
#[derive(Default)]
struct FxHasher(u64);
impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}
impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(last));
        }
    }
    #[inline]
    fn write_u8(&mut self, b: u8) {
        self.add(u64::from(b));
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Keyed by path inside `www`. Every folder with an index.html is also listed under
/// its own path ("" for the top, "docs/" for docs/index.html), sharing the same File,
/// so `/` and `/docs/` are found without building a key per request.
type Files = HashMap<String, Arc<File>, BuildHasherDefault<FxHasher>>;

/// Whether `key` is a folder entry added by `load` rather than a file of its own.
fn is_folder_key(key: &str) -> bool {
    key.is_empty() || key.ends_with('/')
}

fn push_header(h: &mut Vec<u8>, name: &str, value: &str) {
    h.extend_from_slice(name.as_bytes());
    h.extend_from_slice(b": ");
    h.extend_from_slice(value.as_bytes());
    h.extend_from_slice(b"\r\n");
}

fn status_line(h: &mut Vec<u8>, code: StatusCode) {
    h.extend_from_slice(b"HTTP/1.1 ");
    h.extend_from_slice(code.as_str().as_bytes());
    h.push(b' ');
    h.extend_from_slice(code.canonical_reason().unwrap_or("").as_bytes());
    h.extend_from_slice(b"\r\n");
}

/// Headers of a full-file response, in the original's order, without the status line,
/// Content-Length or Date. `is_404_page` uses the original's not_found() header set.
fn file_headers(h: &mut Vec<u8>, f: &File, enc: Option<&str>, content_range: Option<&str>) {
    if f.is_404_page {
        push_header(h, "cache-control", ERROR_CACHE_CONTROL);
        if COMPRESSION {
            push_header(h, "vary", "Accept-Encoding");
        }
    } else {
        if f.ranges {
            push_header(h, "accept-ranges", "bytes");
        }
        if let Some(cr) = content_range {
            push_header(h, "content-range", cr);
        }
        if ETAGS {
            push_header(h, "etag", &f.etag);
        }
        push_header(h, "cache-control", &f.cc);
        if COMPRESSION {
            push_header(h, "vary", "Accept-Encoding");
        }
    }
    for (n, v) in SECURITY {
        push_header(h, n, v);
    }
    push_header(h, "content-type", &f.ctype);
    if let Some(enc) = enc {
        push_header(h, "content-encoding", enc);
    }
}

fn build_variant(f: &File, body: Bytes, enc: Option<&'static str>) -> Variant {
    let mut h = Vec::with_capacity(1024);
    let code = if f.is_404_page {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    status_line(&mut h, code);
    push_header(&mut h, "content-length", &body.len().to_string());
    file_headers(&mut h, f, enc, None);
    h.extend_from_slice(b"date: ");
    Variant {
        body,
        enc,
        head: Bytes::from(h),
    }
}

fn pick_table(f: &File, br: Option<Bytes>) -> PickTable {
    let original = build_variant(f, f.body.clone(), None);
    let brotli = match br {
        Some(c) if c.len() < f.body.len() => build_variant(f, c, Some("br")),
        _ => original.clone(),
    };
    [original, brotli]
}

fn placeholder() -> PickTable {
    std::array::from_fn(|_| Variant {
        body: Bytes::new(),
        enc: None,
        head: Bytes::new(),
    })
}

struct LoadStats {
    cached_files: usize,
    cached_bytes: usize,
    disk_files: usize,
    disk_bytes: usize,
}

fn load(root: &Path, max_in_memory: usize, cache_memory: usize) -> (Files, LoadStats) {
    let mut files = Files::default();
    let mut stats = LoadStats {
        cached_files: 0,
        cached_bytes: 0,
        disk_files: 0,
        disk_bytes: 0,
    };
    // Bytes of file bodies and kept pre-compressed copies. The 404 alias and
    // folder keys added below share those bodies and are not counted again.
    let mut used: usize = 0;
    let walker = walkdir_files(root);
    for (rel, path) in walker {
        let ctype = content_type(&rel);
        let lower = rel.to_ascii_lowercase();
        let base = if lower.ends_with(".html") || lower.ends_with(".htm") {
            HTML_CACHE_CONTROL
        } else {
            ASSET_CACHE_CONTROL
        };
        let cc = if NO_TRANSFORM_ONLY_FOR_PRECOMPRESSED_TYPES && !compressible(&ctype) {
            format!("{base}, no-transform")
        } else {
            base.to_owned()
        };
        let meta_len = path.metadata().ok().map(|m| m.len());
        let too_big = meta_len.is_some_and(|n| n > max_in_memory as u64);
        let remaining = cache_memory.saturating_sub(used);
        let over_budget = meta_len.is_some_and(|n| n > remaining as u64);
        // Unknown length is streamed from disk too: reading it whole is how a
        // site larger than RAM used to get the process killed.
        let (body, disk_path, size, etag_hash, br) = if too_big || over_budget || meta_len.is_none()
        {
            let Ok((hash, size)) = hash_file(&path) else {
                continue;
            };
            (Bytes::new(), Some(path.clone()), size, hash, None)
        } else {
            let Ok(buf) = std::fs::read(&path) else {
                continue;
            };
            if used.saturating_add(buf.len()) > cache_memory {
                let hash = fnv1a(&buf);
                let size = buf.len();
                (Bytes::new(), Some(path.clone()), size, hash, None)
            } else {
                let hash = fnv1a(&buf);
                let size = buf.len();
                let body = Bytes::from(buf);
                // Keep a Brotli copy only when it is smaller and still inside the budget.
                // Otherwise the file stays in RAM uncompressed and ranges still work.
                let br = (COMPRESSION && compressible(&ctype))
                    .then(|| smaller(brotli_of(&body, BROTLI_FAST, BROTLI_MAX_WINDOW), body.len()))
                    .flatten()
                    .filter(|c| used.saturating_add(size).saturating_add(c.len()) <= cache_memory);
                (body, None, size, hash, br)
            }
        };
        if disk_path.is_some() {
            stats.disk_files += 1;
            stats.disk_bytes = stats.disk_bytes.saturating_add(size);
        } else {
            let br_len = br.as_ref().map_or(0, Bytes::len);
            used = used.saturating_add(size).saturating_add(br_len);
            stats.cached_files += 1;
            stats.cached_bytes = used;
        }
        let etag = format!("\"{:016x}\"", etag_hash);
        let ranges = br.is_none();
        let mut nm = Vec::with_capacity(256);
        status_line(&mut nm, StatusCode::NOT_MODIFIED);
        push_header(&mut nm, "etag", &etag);
        push_header(&mut nm, "cache-control", &cc);
        if COMPRESSION {
            push_header(&mut nm, "vary", "Accept-Encoding");
        }
        nm.extend_from_slice(b"date: ");
        let mut f = File {
            ctype,
            cc,
            etag,
            body,
            disk_path,
            size,
            pick: ArcSwap::from_pointee(placeholder()),
            ranges,
            not_modified: Bytes::from(nm),
            is_404_page: false,
        };
        // The 404 page is served under every missing path; its own URL still gets a 200.
        f.pick = ArcSwap::from_pointee(pick_table(&f, br));
        files.insert(rel, Arc::new(f));
    }
    // The alias is not a separate file: it shares 404.html's body (and its bytes).
    if let Some(f) = files.get("404.html") {
        let mut page = File {
            ctype: f.ctype.clone(),
            cc: f.cc.clone(),
            etag: f.etag.clone(),
            body: f.body.clone(),
            disk_path: f.disk_path.clone(),
            size: f.size,
            pick: ArcSwap::from_pointee(placeholder()),
            ranges: f.ranges,
            not_modified: f.not_modified.clone(),
            is_404_page: true,
        };
        let cur = f.pick.load();
        let br = cur[ACCEPT_BR].enc.map(|_| cur[ACCEPT_BR].body.clone());
        page.pick = ArcSwap::from_pointee(pick_table(&page, br));
        files.insert(NOT_FOUND_KEY.to_owned(), Arc::new(page));
    }
    let folders: Vec<(String, Arc<File>)> = files
        .iter()
        .filter_map(|(k, f)| {
            let dir = k.strip_suffix("index.html")?;
            (dir.is_empty() || dir.ends_with('/')).then(|| (dir.to_owned(), Arc::clone(f)))
        })
        .collect();
    files.extend(folders);
    (files, stats)
}

/// Key no request path can produce (requests always decode from a path starting '/').
const NOT_FOUND_KEY: &str = "\x00404";

fn walkdir_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    // `at_root` is only the site root, so `/.well-known/` is served and every
    // other dotfile or dot-directory is skipped.
    let mut stack = vec![(root.to_path_buf(), true)];
    while let Some((dir, at_root)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && !(at_root && name == ".well-known") {
                continue;
            }
            let p = e.path();
            // file_type() does not follow symlinks, so a symlink is neither a
            // plain file nor a directory and is skipped. A link cannot make the
            // server read a path outside ROOT.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push((p, false));
            } else if ft.is_file() {
                let Ok(rel) = p.strip_prefix(root) else {
                    continue;
                };
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, p));
            }
        }
    }
    out
}

fn recompress(files: &Files) {
    let todo: Vec<&File> = files
        .iter()
        .filter(|(k, f)| !is_folder_key(k) && !f.ranges)
        .map(|(_, f)| &**f)
        .collect();
    let next = AtomicUsize::new(0);
    let workers = thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut replaced = 0;
                while let Some(f) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let old = f.pick.load_full();
                    let keep = |new: Option<Vec<u8>>, current: &Variant| {
                        smaller(new, current.body.len())
                            .or_else(|| current.enc.map(|_| current.body.clone()))
                    };
                    let br = keep(
                        brotli_of(&f.body, BROTLI_BEST, brotli_window(f.body.len())),
                        &old[ACCEPT_BR],
                    );
                    f.pick.store(Arc::new(pick_table(f, br)));
                    replaced += old[ACCEPT_BR].enc.map_or(0, |_| old[ACCEPT_BR].body.len());
                    drop(old);
                    if replaced >= RETURN_MEMORY_EVERY {
                        replaced = 0;
                        return_freed_memory_to_the_system();
                    }
                }
            });
        }
    });
    // The 404 alias is recompressed too (its own head), but it is not a file of its own.
    let reported = todo.iter().filter(|f| !f.is_404_page).count();
    println!("recompressed {reported} files at the strongest levels");
}

// ---------------------------------------------------------------- per-thread Date

thread_local! {
    static DATE: RefCell<(u64, [u8; 29])> = const { RefCell::new((0, [0; 29])) };
}

fn push_date_end(out: &mut Vec<u8>) {
    let now = SystemTime::now();
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    DATE.with(|d| {
        let mut d = d.borrow_mut();
        if d.0 != secs {
            let s = httpdate::fmt_http_date(now);
            d.1.copy_from_slice(&s.as_bytes()[..29]);
            d.0 = secs;
        }
        out.extend_from_slice(&d.1);
    });
    out.extend_from_slice(b"\r\n\r\n");
}

// ---------------------------------------------------------------- request handling

struct Reply {
    keep_alive: bool,
    /// Cached file body (reference-counted, not a per-connection copy), written
    /// with writev right after the head.
    big_body: Option<Bytes>,
    /// On-disk file to stream after the head (`offset`, `len`).
    disk: Option<(PathBuf, u64, u64)>,
}

struct Req<'a> {
    method: &'a str,
    target: &'a str,
    http11: bool,
    accept_encoding: &'a [u8],
    if_none_match: Option<&'a [u8]>,
    range: Option<&'a [u8]>,
    if_range: Option<&'a [u8]>,
    close: bool,
    has_body: bool,
    bad: bool,
}

fn read_req<'b>(r: &httparse::Request<'_, 'b>) -> Req<'b> {
    let mut q = Req {
        method: r.method.unwrap_or(""),
        target: r.path.unwrap_or(""),
        http11: r.version == Some(1),
        accept_encoding: b"",
        if_none_match: None,
        range: None,
        if_range: None,
        close: false,
        has_body: false,
        bad: false,
    };
    let (mut hosts, mut lengths, mut keep_alive_token) = (0, 0, false);
    for h in r.headers.iter() {
        let n = h.name;
        if n.eq_ignore_ascii_case("host") {
            hosts += 1;
        } else if n.eq_ignore_ascii_case("accept-encoding") {
            q.accept_encoding = h.value;
        } else if n.eq_ignore_ascii_case("if-none-match") {
            q.if_none_match = Some(h.value);
        } else if n.eq_ignore_ascii_case("range") {
            q.range = Some(h.value);
        } else if n.eq_ignore_ascii_case("if-range") {
            q.if_range = Some(h.value);
        } else if n.eq_ignore_ascii_case("connection") {
            if contains_ignore_case(h.value, b"close") {
                q.close = true;
            }
            if contains_ignore_case(h.value, b"keep-alive") {
                keep_alive_token = true;
            }
        } else if n.eq_ignore_ascii_case("content-length") {
            lengths += 1;
            match std::str::from_utf8(h.value)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
            {
                Some(0) => {}
                Some(_) => q.has_body = true,
                None => q.bad = true,
            }
        } else if n.eq_ignore_ascii_case("transfer-encoding") {
            q.has_body = true;
        }
    }
    // RFC 9112: an HTTP/1.1 request carries exactly one Host; never two in any version.
    // Two Content-Length headers are refused outright, as actix does.
    if (q.http11 && hosts != 1) || hosts > 1 || lengths > 1 {
        q.bad = true;
    }
    // HTTP/1.0 keeps the connection only when it asks for keep-alive.
    if !q.http11 && !keep_alive_token {
        q.close = true;
    }
    q
}

fn accept_mask(ae: &[u8]) -> usize {
    if !COMPRESSION {
        return 0;
    }
    // What Cloudflare sends on every request; skip parsing it.
    if ae == b"gzip, br" || ae == b"br, gzip" {
        return ACCEPT_BR;
    }
    accept_mask_of(std::str::from_utf8(ae).unwrap_or(""))
}

/// Whether the request accepts Brotli, per RFC 9110 §12.5.3: codings are
/// case-insensitive, `;q=0` refuses one, and `*` stands for every coding not named.
fn accept_mask_of(ae: &str) -> usize {
    let (mut br, mut any) = (None, None);
    for item in ae.split(',') {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or("").trim();
        let refused = parts.any(|p| {
            let p = p.trim();
            // get() rather than [..2]: a non-ASCII byte must not panic (panic = abort)
            p.get(..2).is_some_and(|k| k.eq_ignore_ascii_case("q="))
                && p.get(2..)
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .is_some_and(|q| q <= 0.0)
        });
        let slot = if coding.eq_ignore_ascii_case("br") {
            &mut br
        } else if coding == "*" {
            &mut any
        } else {
            continue;
        };
        *slot = Some(!refused);
    }
    if br.or(any).unwrap_or(false) {
        ACCEPT_BR
    } else {
        0
    }
}

/// Writes a response that was not prebuilt: status, Content-Length, given headers, Date.
fn dynamic(out: &mut Vec<u8>, code: StatusCode, headers: &[(&str, &str)], body: &[u8], head: bool) {
    status_line(out, code);
    push_header(out, "content-length", &body.len().to_string());
    for (n, v) in headers {
        push_header(out, n, v);
    }
    out.extend_from_slice(b"date: ");
    push_date_end(out);
    if !head {
        out.extend_from_slice(body);
    }
}

/// Error responses in actix's format: empty body and `connection: close`.
fn bare_error(out: &mut Vec<u8>, code: StatusCode) {
    status_line(out, code);
    out.extend_from_slice(b"content-length: 0\r\nconnection: close\r\ndate: ");
    push_date_end(out);
}

/// HTTP/1.0 requests get an HTTP/1.0 status line, as actix sends; a response that ends
/// the connection says `connection: close`, and an HTTP/1.0 keep-alive one says so.
/// Only these rare responses are touched; keep-alive HTTP/1.1 replies are left as built.
fn finalize(out: &mut Vec<u8>, start: usize, http11: bool, keep_alive: bool) {
    if !out[start..].starts_with(b"HTTP/1.1 ") {
        return;
    }
    if !http11 {
        out[start + 5..start + 8].copy_from_slice(b"1.0");
    }
    let header: &[u8] = if !keep_alive && !http11 {
        return; // closing is HTTP/1.0's default; actix adds no header either
    } else if !keep_alive {
        b"connection: close\r\n"
    } else if !http11 {
        b"connection: keep-alive\r\n"
    } else {
        return;
    };
    let Some(end) = out[start..].windows(4).position(|w| w == b"\r\n\r\n") else {
        return;
    };
    let head = &out[start..start + end];
    if head
        .windows(13)
        .any(|w| w.eq_ignore_ascii_case(b"\r\nconnection:"))
    {
        return;
    }
    let at = start + end + 2;
    out.splice(at..at, header.iter().copied());
}

fn plain(out: &mut Vec<u8>, code: StatusCode, cc: &str, extra: &[(&str, &str)], head: bool) {
    let mut h = vec![
        ("content-type", "text/plain; charset=utf-8"),
        ("cache-control", cc),
    ];
    h.extend_from_slice(extra);
    dynamic(
        out,
        code,
        &h,
        code.canonical_reason().unwrap_or("").as_bytes(),
        head,
    );
}

fn send_variant(out: &mut Vec<u8>, v: &Variant, head: bool) -> Option<Bytes> {
    out.extend_from_slice(&v.head);
    push_date_end(out);
    if head || v.body.is_empty() {
        None
    } else {
        Some(v.body.clone())
    }
}

fn handle(files: &Files, q: &Req, out: &mut Vec<u8>) -> Reply {
    let head = q.method == "HEAD";
    let mut keep = !q.close;
    let done = |keep| Reply {
        keep_alive: keep,
        big_body: None,
        disk: None,
    };

    if q.bad {
        bare_error(out, StatusCode::BAD_REQUEST);
        return done(false);
    }
    if q.method != "GET" && !head {
        // A request body is never read, so the connection cannot be reused after one.
        if q.has_body {
            keep = false;
        }
        const OTHER: [&str; 7] = [
            "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "CONNECT",
        ];
        if OTHER.contains(&q.method) {
            plain(
                out,
                StatusCode::METHOD_NOT_ALLOWED,
                ERROR_CACHE_CONTROL,
                &[("allow", "GET, HEAD")],
                false,
            );
        } else {
            dynamic(
                out,
                StatusCode::NOT_IMPLEMENTED,
                &[("content-type", "text/plain; charset=utf-8")],
                b"Not Implemented",
                false,
            );
        }
        return done(keep);
    }
    // RFC 9112 §3.2.2: accept absolute-form ("http://host/path") as well as origin-form.
    // A #fragment is never part of the resource (browsers do not send one; actix drops it).
    let raw_target = q.target.split('#').next().unwrap_or("");
    let target = match raw_target.split_once("://") {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            rest.find('/').map_or("/", |i| &rest[i..])
        }
        _ => raw_target,
    };
    if q.has_body || !target.starts_with('/') {
        bare_error(out, StatusCode::BAD_REQUEST);
        return done(false);
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let p = percent_encoding::percent_decode_str(path).decode_utf8_lossy();

    if let Some(dir) = p.strip_suffix("index.html").filter(|d| d.ends_with('/')) {
        let loc = redirect_target(dir) + &query_suffix(query);
        plain(
            out,
            StatusCode::MOVED_PERMANENTLY,
            REDIRECT_CACHE_CONTROL,
            &[("location", loc.as_str())],
            head,
        );
        return done(keep);
    }

    let key = p.strip_prefix('/').unwrap_or(&p);
    let Some(f) = files.get(key) else {
        if !p.ends_with('/') && files.contains_key(&format!("{key}/")) {
            let loc = redirect_target(&format!("{p}/")) + &query_suffix(query);
            plain(
                out,
                StatusCode::MOVED_PERMANENTLY,
                REDIRECT_CACHE_CONTROL,
                &[("location", loc.as_str())],
                head,
            );
            return done(keep);
        }
        return match files.get(NOT_FOUND_KEY) {
            Some(page) if page.disk_path.is_some() => send_disk_file(out, page, q, head, keep),
            Some(page) => {
                let t = page.pick.load();
                let big = send_variant(out, &t[accept_mask(q.accept_encoding)], head);
                Reply {
                    keep_alive: keep,
                    big_body: big,
                    disk: None,
                }
            }
            None => {
                plain(out, StatusCode::NOT_FOUND, ERROR_CACHE_CONTROL, &[], head);
                done(keep)
            }
        };
    };

    if ETAGS
        && q.if_none_match
            .is_some_and(|inm| contains(inm, f.etag.as_bytes()))
    {
        out.extend_from_slice(&f.not_modified);
        push_date_end(out);
        return done(keep);
    }

    if f.disk_path.is_some() {
        return send_disk_file(out, f, q, head, keep);
    }

    let t = f.pick.load();
    let v = &t[accept_mask(q.accept_encoding)];

    if let Some(range) = q.range.filter(|_| f.ranges && !head) {
        let current = q
            .if_range
            .is_none_or(|ir| ir.is_empty() || (ETAGS && ir == f.etag.as_bytes()));
        if let (true, Ok(range)) = (current, std::str::from_utf8(range)) {
            let len = v.body.len();
            match parse_range(range, len) {
                ByteRange::Part(start, end) => {
                    let cr = format!("bytes {start}-{end}/{len}");
                    let part = v.body.slice(start..=end);
                    status_line(out, StatusCode::PARTIAL_CONTENT);
                    push_header(out, "content-length", &part.len().to_string());
                    file_headers(out, f, v.enc, Some(&cr));
                    out.extend_from_slice(b"date: ");
                    push_date_end(out);
                    return Reply {
                        keep_alive: keep,
                        big_body: Some(part),
                        disk: None,
                    };
                }
                ByteRange::Unsatisfiable => {
                    let cr = format!("bytes */{len}");
                    plain(
                        out,
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        ERROR_CACHE_CONTROL,
                        &[("accept-ranges", "bytes"), ("content-range", cr.as_str())],
                        false,
                    );
                    return done(keep);
                }
                ByteRange::Ignore => {}
            }
        }
    }

    let big = send_variant(out, v, head);
    Reply {
        keep_alive: keep,
        big_body: big,
        disk: None,
    }
}

fn send_disk_file(out: &mut Vec<u8>, f: &File, q: &Req, head: bool, keep: bool) -> Reply {
    let path = f.disk_path.as_ref().expect("disk file").clone();
    let mut start = 0u64;
    let mut len = f.size as u64;
    let mut code = if f.is_404_page {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    let mut cr: Option<String> = None;
    if let Some(range) = q.range.filter(|_| f.ranges && !head) {
        let current = q
            .if_range
            .is_none_or(|ir| ir.is_empty() || (ETAGS && ir == f.etag.as_bytes()));
        if let (true, Ok(range)) = (current, std::str::from_utf8(range)) {
            match parse_range(range, f.size) {
                ByteRange::Part(s, e) => {
                    cr = Some(format!("bytes {s}-{e}/{}", f.size));
                    start = s as u64;
                    len = (e - s + 1) as u64;
                    code = StatusCode::PARTIAL_CONTENT;
                }
                ByteRange::Unsatisfiable => {
                    let cr = format!("bytes */{}", f.size);
                    plain(
                        out,
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        ERROR_CACHE_CONTROL,
                        &[("accept-ranges", "bytes"), ("content-range", cr.as_str())],
                        false,
                    );
                    return Reply {
                        keep_alive: keep,
                        big_body: None,
                        disk: None,
                    };
                }
                ByteRange::Ignore => {}
            }
        }
    }
    status_line(out, code);
    push_header(out, "content-length", &len.to_string());
    file_headers(out, f, None, cr.as_deref());
    out.extend_from_slice(b"date: ");
    push_date_end(out);
    if head || len == 0 {
        return Reply {
            keep_alive: keep,
            big_body: None,
            disk: None,
        };
    }
    Reply {
        keep_alive: keep,
        big_body: None,
        disk: Some((path, start, len)),
    }
}

async fn stream_disk(s: &mut TcpStream, path: &Path, offset: u64, len: u64) -> bool {
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return false,
    };
    if offset > 0 && f.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return false;
    }
    let mut left = len;
    let mut buf = vec![0u8; 64 * 1024];
    while left > 0 {
        let n = std::cmp::min(left, buf.len() as u64) as usize;
        let n = match timeout(WRITE_TIMEOUT, f.read(&mut buf[..n])).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return false,
            Ok(Ok(n)) => n,
        };
        if write_two(s, &buf[..n], &[]).await.is_err() {
            return false;
        }
        left -= n as u64;
    }
    true
}

// ---------------------------------------------------------------- connection loop

/// WRITE_TIMEOUT limits each write call (time without progress), not the whole
/// response, so a slow but steady reader can still receive a large file.
async fn write_two(s: &mut TcpStream, mut a: &[u8], mut b: &[u8]) -> io::Result<()> {
    while !a.is_empty() || !b.is_empty() {
        let write = async {
            if a.is_empty() {
                s.write(b).await
            } else if b.is_empty() {
                s.write(a).await
            } else {
                s.write_vectored(&[IoSlice::new(a), IoSlice::new(b)]).await
            }
        };
        let n = timeout(WRITE_TIMEOUT, write)
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        let k = n.min(a.len());
        a = &a[k..];
        b = &b[n - k..];
    }
    Ok(())
}

async fn flush(s: &mut TcpStream, out: &mut Vec<u8>, body: &[u8]) -> bool {
    let ok = write_two(s, out, body).await.is_ok();
    out.clear();
    shrink_idle(out);
    ok
}

/// Drop spare capacity above `CONN_BUF_KEEP` (or just past a partial header).
fn shrink_idle(buf: &mut Vec<u8>) {
    let keep = if buf.len() <= CONN_BUF_KEEP {
        CONN_BUF_KEEP
    } else {
        buf.len().saturating_add(CONN_BUF_KEEP).min(MAX_HEAD)
    };
    if buf.capacity() > keep {
        buf.shrink_to(keep);
    }
}

/// Leave room for one small read, and never past `MAX_HEAD`.
fn prepare_read(buf: &mut Vec<u8>) {
    let room = MAX_HEAD.saturating_sub(buf.len());
    let chunk = room.min(CONN_BUF_KEEP);
    if chunk == 0 {
        return;
    }
    let spare = buf.capacity().saturating_sub(buf.len());
    if spare > chunk {
        buf.shrink_to(buf.len() + chunk);
    } else if spare < chunk {
        buf.reserve(chunk);
    }
}

async fn connection(
    mut s: TcpStream,
    files: &'static Files,
    mut stop: watch::Receiver<bool>,
    _counted: Counted,
) {
    // On Linux, TCP_NODELAY, TCP_USER_TIMEOUT and TCP_NOTSENT_LOWAT are set once on the
    // listening socket and every accepted socket inherits them, so accepting costs no
    // extra system calls. Elsewhere, set what is portable here.
    #[cfg(not(target_os = "linux"))]
    let _ = s.set_nodelay(true);
    let mut charge = HeadCharge(0);
    let mut buf: Vec<u8> = Vec::with_capacity(CONN_BUF_KEEP);
    let mut out: Vec<u8> = Vec::with_capacity(CONN_BUF_KEEP);
    // Until a first request is answered, a silent connection gets the request-head
    // timeout (as actix's client_request_timeout), not the keep-alive idle one.
    let mut answered = false;
    let mut requests: usize = 0;
    // When the first bytes of a request head that is still incomplete arrived. The whole
    // head must arrive within REQUEST_HEAD_TIMEOUT, so a client that sends it a little
    // at a time (slowloris) cannot hold the connection open forever.
    let mut head_started: Option<Instant> = None;
    loop {
        // Answer every complete request already buffered (pipelining keeps order).
        let mut pos = 0;
        loop {
            // Left uninitialised: httparse only reads the slots it has filled.
            let mut hs = [const { MaybeUninit::<httparse::Header>::uninit() }; MAX_HEADERS];
            let mut r = httparse::Request::new(&mut []);
            let (consumed, reply) = match r.parse_with_uninit_headers(&buf[pos..], &mut hs) {
                // A head that arrived whole in one read can be longer than MAX_HEAD; refuse
                // it like one still coming in, so the limit always holds.
                Ok(httparse::Status::Complete(n)) if n > MAX_HEAD => {
                    bare_error(&mut out, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
                    let _ = flush(&mut s, &mut out, &[]).await;
                    return;
                }
                Ok(httparse::Status::Complete(n)) => {
                    let q = read_req(&r);
                    let start = out.len();
                    let mut reply = handle(files, &q, &mut out);
                    requests += 1;
                    if requests >= MAX_KEEPALIVE_REQUESTS {
                        reply.keep_alive = false;
                    }
                    finalize(&mut out, start, q.http11, reply.keep_alive);
                    (n, reply)
                }
                Ok(httparse::Status::Partial) => break,
                Err(e) => {
                    let code = if matches!(e, httparse::Error::TooManyHeaders) {
                        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    bare_error(&mut out, code);
                    let _ = flush(&mut s, &mut out, &[]).await;
                    return;
                }
            };
            pos += consumed;
            answered = true;
            if let Some(body) = reply.big_body {
                if !flush(&mut s, &mut out, &body).await {
                    return;
                }
            } else if let Some((path, off, len)) = reply.disk {
                if !flush(&mut s, &mut out, &[]).await {
                    return;
                }
                if !stream_disk(&mut s, &path, off, len).await {
                    return;
                }
            }
            if !reply.keep_alive {
                let _ = flush(&mut s, &mut out, &[]).await;
                return;
            }
            if out.len() >= FLUSH_AT && !flush(&mut s, &mut out, &[]).await {
                return;
            }
        }
        if pos > 0 {
            buf.drain(..pos);
            head_started = None;
        }
        shrink_idle(&mut buf);
        if !out.is_empty() && !flush(&mut s, &mut out, &[]).await {
            return;
        }
        if buf.len() >= MAX_HEAD {
            bare_error(&mut out, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
            let _ = flush(&mut s, &mut out, &[]).await;
            return;
        }
        // Everything answered so far has been written; during shutdown, close here.
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            return;
        }
        let wait = if buf.is_empty() {
            head_started = None;
            if answered {
                Duration::from_secs(KEEP_ALIVE_SECS.load(Ordering::Relaxed))
            } else {
                REQUEST_HEAD_TIMEOUT
            }
        } else {
            let started = *head_started.get_or_insert_with(Instant::now);
            match REQUEST_HEAD_TIMEOUT.checked_sub(started.elapsed()) {
                Some(left) if !left.is_zero() => left,
                _ => return,
            }
        };
        // Idle between requests (and before the first one): hold no buffer, and
        // wait out the keep-alive or header timeout — or shutdown — before allocating.
        if buf.is_empty() {
            buf = Vec::new();
            out = Vec::new();
            let _ = charge.set(0);
            tokio::select! {
                r = timeout(wait, s.readable()) => match r {
                    Ok(Ok(())) => {}
                    _ => return,
                },
                _ = stop.changed() => return,
            }
        }
        prepare_read(&mut buf);
        // Capacity past the two normal buffers is unfinished-header memory.
        let want = if buf.is_empty() {
            0
        } else {
            buf.capacity().saturating_sub(2 * CONN_BUF_KEEP)
        };
        if !charge.set(want) {
            bare_error(&mut out, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
            let _ = flush(&mut s, &mut out, &[]).await;
            return;
        }
        tokio::select! {
            r = timeout(wait, s.read_buf(&mut buf)) => match r {
                Ok(Ok(n)) if n > 0 => {}
                _ => return,
            },
            _ = stop.changed() => return,
        }
    }
}

fn listener(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        s.set_only_v6(false)?; // "::" also takes IPv4, like the original's default
    }
    s.set_reuse_address(true)?;
    enable_reuse_port(&s)?;
    s.set_tcp_nodelay(true)?;
    // Data copied into the kernel send buffer is still un-acked if the client
    // stops reading. Abort that socket after WRITE_TIMEOUT so the buffer is
    // dropped instead of sitting in FIN_WAIT until the system retry limit.
    // Accepted sockets inherit both options from this listener.
    #[cfg(target_os = "linux")]
    {
        s.set_tcp_user_timeout(Some(WRITE_TIMEOUT))?;
        s.set_tcp_notsent_lowat(NOTSENT_LOWAT)?;
    }
    s.bind(&addr.into())?;
    // The kernel clamps this to net.core.somaxconn, so the queue is always as long
    // as the system allows (65535 here) instead of a value fixed in the code.
    s.listen(i32::MAX)?;
    // Blocking: a dedicated thread sits in accept() (see `acceptor`).
    Ok(s.into())
}

/// accept4() with SOCK_NONBLOCK | SOCK_CLOEXEC: one system call per connection.
#[cfg(target_os = "linux")]
fn accept_one(l: &std::net::TcpListener) -> io::Result<std::net::TcpStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let fd = unsafe {
        libc::accept4(
            l.as_raw_fd(),
            std::ptr::null_mut::<libc::sockaddr>(),
            std::ptr::null_mut::<libc::socklen_t>(),
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { std::net::TcpStream::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
fn accept_one(l: &std::net::TcpListener) -> io::Result<std::net::TcpStream> {
    let (s, _) = l.accept()?;
    s.set_nonblocking(true)?;
    Ok(s)
}

/// Minimal overload reply. Written once, non-blocking, then the socket is dropped.
const OVERLOAD_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
Connection: close\r\n\
Retry-After: 1\r\n\
Content-Length: 0\r\n\
\r\n";

fn reject_overloaded(s: std::net::TcpStream) {
    // The accepted socket is already non-blocking. One short write fits in a new
    // socket's send buffer, and a client that never reads cannot stall accept().
    let _ = std::io::Write::write_all(&mut &s, OVERLOAD_RESPONSE);
}

/// SO_REUSEPORT exists on Linux, macOS and the BSDs, which is what lets each core
/// bind the same port. Windows (and the Unixes socket2 leaves out) use one listener.
fn enable_reuse_port(s: &Socket) -> io::Result<()> {
    #[cfg(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx",
            target_os = "wasi",
        ))
    ))]
    s.set_reuse_port(true)?;
    #[cfg(not(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx",
            target_os = "wasi",
        ))
    )))]
    let _ = s;
    Ok(())
}

fn worker_count() -> usize {
    #[cfg(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx",
            target_os = "wasi",
        ))
    ))]
    {
        thread::available_parallelism().map_or(1, |n| n.get())
    }
    #[cfg(not(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx",
            target_os = "wasi",
        ))
    )))]
    {
        1
    }
}

#[cfg(unix)]
fn too_many_files(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENFILE) | Some(libc::EMFILE))
}

/// Pause after EMFILE/ENFILE before accept() again. Long enough that a full
/// file table does not burn a core, short enough that a freed descriptor is
/// picked up quickly.
#[cfg(unix)]
const ACCEPT_FD_BACKOFF: Duration = Duration::from_millis(10);

/// One thread per listening socket that does nothing but accept. It never waits on
/// the worker that serves the connections, so a busy worker cannot leave the kernel's
/// accept queue full, and the OS scheduler always gives it CPU time of its own.
/// With no cap, every accepted connection is served. With `MAX_CONNECTIONS` set,
/// a connection past that cap is answered with a short 503 and closed. EMFILE and
/// ENFILE wait briefly and accept again, so running out of descriptors does not
/// stop the listener or spin a core.
fn acceptor(
    l: std::net::TcpListener,
    worker: tokio::runtime::Handle,
    files: &'static Files,
    stop: watch::Receiver<bool>,
    max_connections: Option<usize>,
) {
    loop {
        match accept_one(&l) {
            Ok(s) => {
                if SHUTTING_DOWN.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(counted) = Counted::try_new(max_connections) {
                    let stop = stop.clone();
                    worker.spawn(async move {
                        if let Ok(s) = TcpStream::from_std(s) {
                            connection(s, files, stop, counted).await;
                        }
                    });
                } else {
                    reject_overloaded(s);
                }
            }
            Err(_) if SHUTTING_DOWN.load(Ordering::Relaxed) => break,
            #[cfg(unix)]
            Err(e) if too_many_files(&e) => {
                // EMFILE / ENFILE: out of file descriptors. Wait for one to close,
                // then accept again. The listener stays up, and this thread does not spin.
                thread::sleep(ACCEPT_FD_BACKOFF);
            }
            Err(_) => {} // ECONNABORTED, EINTR, ...: just accept the next one
        }
    }
}

fn return_large_blocks_when_freed() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_TRIM_THRESHOLD: i32 = -1;
        const M_MMAP_THRESHOLD: i32 = -3;
        unsafe {
            mallopt(M_TRIM_THRESHOLD, 128 * 1024);
            mallopt(M_MMAP_THRESHOLD, 128 * 1024);
        }
    }
}

fn return_freed_memory_to_the_system() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}

fn main() {
    let root = env_str("ROOT", "/root/www");
    let port: u16 = env_str("PORT", "80").parse().unwrap_or_else(|e| {
        eprintln!("bad PORT: {e}");
        std::process::exit(1);
    });
    KEEP_ALIVE_SECS.store(env_usize("KEEP_ALIVE", 120).max(1) as u64, Ordering::Relaxed);
    let max_in_memory = env_usize("MAX_IN_MEMORY", DEFAULT_MAX_IN_MEMORY);
    let usable = usable_memory();
    // No connection cap unless MAX_CONNECTIONS is a positive number. Unset or 0
    // is unlimited; the process is then bounded by open files and memory.
    let max_connections = env_max_connections();
    let max_connections_label = max_connections_label(max_connections);
    HEAD_BUDGET.store(env_usize("HEAD_MEMORY", usable / 16), Ordering::Relaxed);
    println!(
        "usable memory {} MiB, max connections {max_connections_label}, head budget {} MiB",
        usable >> 20,
        HEAD_BUDGET.load(Ordering::Relaxed) >> 20,
    );
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(10));
        let n = OPEN_CONNECTIONS.load(Ordering::Relaxed);
        let peak = PEAK_SINCE_TRIM.load(Ordering::Relaxed);
        if peak > n + 512 {
            PEAK_SINCE_TRIM.store(n, Ordering::Relaxed);
            return_freed_memory_to_the_system();
        }
    });
    let cache_memory = env_bytes("CACHE_MEMORY", usable / 4);
    raise_nofile();

    let host = std::env::var("HOST")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            if std::net::TcpListener::bind(("::", 0)).is_ok() {
                "::".to_owned()
            } else {
                "0.0.0.0".to_owned()
            }
        });
    let addr: SocketAddr = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port),
        Err(_) => {
            eprintln!("bad HOST: {host}");
            std::process::exit(1);
        }
    };
    let n = worker_count();

    // Bind before reading the site. A port that is taken or not allowed to us
    // fails here, instead of after the process has pulled every file into RAM.
    // SO_REUSEPORT would let this process share a port another server already holds,
    // splitting visitors between them. A plain bind fails if anything listens there.
    if let Err(e) = probe(addr) {
        die_listen(port, &e);
    }
    let listeners: Vec<std::net::TcpListener> = (0..n)
        .map(|_| match listener(addr) {
            Ok(l) => l,
            Err(e) => die_listen(port, &e),
        })
        .collect();

    let (loaded, stats) = load(Path::new(&root), max_in_memory, cache_memory);
    let files: &'static Files = Box::leak(Box::new(loaded));
    let total = stats.cached_files + stats.disk_files;
    if total == 0 {
        eprintln!("no files found under {root}");
        std::process::exit(1);
    }
    println!(
        "loaded {total} files from {root} (cache budget {}): {} cached ({}), {} served from disk ({})",
        fmt_bytes(cache_memory as u64),
        stats.cached_files,
        fmt_bytes(stats.cached_bytes as u64),
        stats.disk_files,
        fmt_bytes(stats.disk_bytes as u64),
    );
    println!(
        "max in-memory file: {max_in_memory} bytes, max connections: {max_connections_label}, header timeout: {}s, keep-alive idle: {}s, write timeout: {}s, max requests per connection: {MAX_KEEPALIVE_REQUESTS}",
        REQUEST_HEAD_TIMEOUT.as_secs(),
        KEEP_ALIVE_SECS.load(Ordering::Relaxed),
        WRITE_TIMEOUT.as_secs(),
    );
    return_large_blocks_when_freed();
    return_freed_memory_to_the_system();
    if COMPRESSION {
        thread::spawn(move || {
            recompress(files);
            return_freed_memory_to_the_system();
        });
    }
    println!("listening on {addr} with {n} per-core workers");

    // One stop channel per worker, so a connection waiting for its next request only
    // touches its own core's channel, never a lock shared with the other cores.
    let mut stop_txs = Vec::with_capacity(n);
    #[cfg(unix)]
    let listener_fds: Vec<i32> = {
        use std::os::fd::AsRawFd;
        listeners.iter().map(|l| l.as_raw_fd()).collect()
    };
    #[cfg(windows)]
    let listener_socks: Vec<std::os::windows::raw::SOCKET> = {
        use std::os::windows::io::AsRawSocket;
        listeners.iter().map(|l| l.as_raw_socket()).collect()
    };
    let mut acceptors = Vec::with_capacity(n);
    let threads: Vec<_> = listeners
        .into_iter()
        .map(|std_listener| {
            let (stop_tx, mut stop) = watch::channel(false);
            stop_txs.push(stop_tx);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let worker = rt.handle().clone();
            let conn_stop = stop.clone();
            // A task spawned before this runtime is driving is queued, and that
            // first wakeup is lost, so the connection sits unread until another
            // one lands on this worker. Accept only after the runtime is polling.
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            acceptors.push(thread::spawn(move || {
                let _ = ready_rx.recv();
                acceptor(std_listener, worker, files, conn_stop, max_connections)
            }));
            thread::spawn(move || {
                rt.block_on(async move {
                    tokio::task::yield_now().await;
                    let _ = ready_tx.send(());
                    let _ = stop.changed().await;
                    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
                    while OPEN_CONNECTIONS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
            })
        })
        .collect();

    // SIGTERM (systemctl stop, kill) or SIGINT (Ctrl+C) on Unix. Ctrl+C elsewhere.
    wait_for_shutdown_signal();
    println!(
        "shutting down: finishing in-flight requests (up to {} s)",
        SHUTDOWN_TIMEOUT.as_secs()
    );
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    // Wake the acceptor threads out of accept(); each then closes its listener.
    #[cfg(unix)]
    for fd in &listener_fds {
        unsafe {
            libc::shutdown(*fd, libc::SHUT_RD);
        }
    }
    #[cfg(windows)]
    wake_windows_listeners(&listener_socks);
    for a in acceptors {
        let _ = a.join();
    }
    for stop_tx in &stop_txs {
        let _ = stop_tx.send(true);
    }
    for t in threads {
        let _ = t.join();
    }
}

/// Bind without SO_REUSEPORT to find out whether another process holds the port.
fn probe(addr: SocketAddr) -> io::Result<()> {
    let s = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        s.set_only_v6(false)?;
    }
    s.set_reuse_address(true)?;
    s.bind(&addr.into())?;
    s.listen(1)?;
    Ok(())
}

fn permission_denied(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EACCES)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

fn addr_in_use(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::AddrInUse {
        return true;
    }
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EADDRINUSE)
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    }
}

fn die_listen(port: u16, e: &io::Error) -> ! {
    if permission_denied(e) {
        let bin = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<binary>".to_owned());
        eprintln!(
            "listen on :{port}: permission denied\n\
             run as root, `sudo setcap cap_net_bind_service=+ep {bin}`, or set PORT=8080 behind a reverse proxy"
        );
    } else if addr_in_use(e) {
        eprintln!("listen on :{port}: port {port} is already in use");
    } else {
        eprintln!("listen on :{port}: {e}");
    }
    std::process::exit(1);
}

fn raise_nofile() {
    // Raise the soft open-file limit to the hard limit. The rlimit helper is a
    // no-op where RLIMIT_NOFILE does not exist, including Windows.
    #[cfg(unix)]
    {
        match rlimit::increase_nofile_limit(rlimit::INFINITY) {
            Ok(n) => {
                if n >= rlimit::INFINITY {
                    println!("open file limit: unlimited");
                } else {
                    println!("open file limit: {n}");
                }
            }
            Err(e) => eprintln!("could not raise open file limit: {e}"),
        }
    }
}

fn wait_for_shutdown_signal() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
        });
}

#[cfg(windows)]
fn wake_windows_listeners(handles: &[std::os::windows::raw::SOCKET]) {
    #[link(name = "ws2_32")]
    extern "system" {
        fn shutdown(s: std::os::windows::raw::SOCKET, how: i32) -> i32;
    }
    // SD_RECEIVE: unblock accept(), same as SHUT_RD on Unix.
    for sock in handles {
        unsafe {
            shutdown(*sock, 0);
        }
    }
}
