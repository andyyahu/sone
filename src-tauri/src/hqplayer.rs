//! Hand a decoded track to an already-running HQPlayer Desktop.
//!
//! SONE keeps the TIDAL session and the PCM decode. Desktop owns the filters,
//! the noise shaper, and the DAC. The handoff is one sized WAV on 127.0.0.1
//! plus a `PlayNextURI` on the XML control port (4321). Reads block until the
//! decoder has produced those bytes, so a client never receives a zero-filled
//! tail while the track is still decoding.

use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_PORT: u16 = 4321;
const MAX_WAV_DATA: u32 = u32::MAX - 36;
const MAX_HTTP_CLIENTS: usize = 4;
const MAX_PREROLL_BYTES: usize = 8 * 1024 * 1024;
const HTTP_WAIT_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) enum ControlError {
    Transport(String),
    Rejected(String),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Transport(msg) | ControlError::Rejected(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ControlError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EngineInfo {
    pub product: String,
    pub engine: String,
    pub version: String,
    pub platform: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Status {
    pub state: i32,
    pub position: f64,
    pub length: f64,
}

pub(crate) struct ControlSession {
    stream: TcpStream,
    pending: Vec<u8>,
}

impl ControlSession {
    pub(crate) fn connect(host: &str, port: u16) -> Result<Self, ControlError> {
        let addrs = loopback_addrs(host, port)?;
        let stream = dial_addrs(&addrs)?;
        Ok(Self {
            stream,
            pending: Vec::new(),
        })
    }

    pub(crate) fn get_info(&mut self) -> Result<EngineInfo, ControlError> {
        let doc = self.roundtrip("<GetInfo/>")?;
        Ok(EngineInfo {
            product: attr(&doc, "product").unwrap_or_default(),
            engine: attr(&doc, "engine").unwrap_or_default(),
            version: attr(&doc, "version").unwrap_or_default(),
            platform: attr(&doc, "platform").unwrap_or_default(),
        })
    }

    pub(crate) fn status(&mut self) -> Result<Status, ControlError> {
        let doc = self.roundtrip("<Status subscribe=\"0\"/>")?;
        let state = attr(&doc, "state")
            .and_then(|value| value.parse::<i32>().ok())
            .ok_or_else(|| ControlError::Rejected("HQPlayer status has no valid state".into()))?;
        let number = |name| -> Result<f64, ControlError> {
            attr(&doc, name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or_else(|| {
                    ControlError::Rejected(format!("HQPlayer status has invalid {name}"))
                })
        };
        Ok(Status {
            state,
            position: number("position")?,
            length: number("length")?,
        })
    }

    pub(crate) fn stop(&mut self) -> Result<(), ControlError> {
        self.roundtrip("<Stop/>").map(|_| ())
    }

    pub(crate) fn playlist_clear(&mut self) -> Result<(), ControlError> {
        self.roundtrip("<PlaylistClear/>").map(|_| ())
    }

    pub(crate) fn play(&mut self) -> Result<(), ControlError> {
        self.roundtrip("<Play/>").map(|_| ())
    }

    pub(crate) fn pause(&mut self) -> Result<(), ControlError> {
        self.roundtrip("<Pause/>").map(|_| ())
    }

    /// Desktop 5.17 reads the address from `value`. An `uri` attribute is
    /// rejected as `clXmlElement::GetAttribute("value"): not found` and the
    /// playlist stays empty.
    pub(crate) fn play_next_uri(&mut self, uri: &str) -> Result<(), ControlError> {
        self.roundtrip(&format!("<PlayNextURI value=\"{}\"/>", xml_escape(uri)))
            .map(|_| ())
    }

    /// Drop one queued playlist row. Index 0 is the track Desktop is playing;
    /// the handoff queues the next track at index 1.
    pub(crate) fn playlist_remove(&mut self, index: u32) -> Result<(), ControlError> {
        self.roundtrip(&format!("<PlaylistRemove index=\"{index}\"/>"))
            .map(|_| ())
    }

    fn roundtrip(&mut self, body: &str) -> Result<String, ControlError> {
        let mut msg = String::with_capacity(body.len() + 48);
        msg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
        msg.push_str(body);
        msg.push('\n');
        self.stream
            .write_all(msg.as_bytes())
            .map_err(|err| ControlError::Transport(err.to_string()))?;
        self.stream
            .flush()
            .map_err(|err| ControlError::Transport(err.to_string()))?;
        let doc = self.read_document(Duration::from_secs(3))?;
        if root_name(&doc) != root_name(body) {
            return Err(ControlError::Transport(
                "unexpected HQPlayer control response".into(),
            ));
        }
        ensure_ok(&doc)?;
        Ok(doc)
    }

    fn read_document(&mut self, budget: Duration) -> Result<String, ControlError> {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(text) = std::str::from_utf8(&self.pending) {
                if let Some((doc, rest)) = split_document(text) {
                    self.pending = rest.as_bytes().to_vec();
                    return Ok(doc);
                }
            }
            if Instant::now() >= deadline {
                return Err(ControlError::Transport("control response timed out".into()));
            }
            if self.pending.len() > 1_000_000 {
                return Err(ControlError::Transport("control response too large".into()));
            }
            let mut tmp = [0u8; 4096];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(ControlError::Transport("control connection closed".into())),
                Ok(n) => self.pending.extend_from_slice(&tmp[..n]),
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(err) => return Err(ControlError::Transport(err.to_string())),
            }
        }
    }
}

/// Resolve only literal loopback addresses; never perform DNS for a remote host.
fn loopback_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>, ControlError> {
    if port == 0 {
        return Err(ControlError::Rejected(
            "HQPlayer control port must be nonzero".into(),
        ));
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
        ]);
    }
    let address = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .map_err(|_| ControlError::Rejected("only local HQPlayer Desktop is supported".into()))?;
    if !address.is_loopback() {
        return Err(ControlError::Rejected(
            "only local HQPlayer Desktop is supported".into(),
        ));
    }
    Ok(vec![SocketAddr::new(address, port)])
}

/// Try both local address families because Desktop may bind only one.
fn dial_addrs(addrs: &[SocketAddr]) -> Result<TcpStream, ControlError> {
    let mut last = ControlError::Transport("no address".into());
    for addr in addrs {
        match TcpStream::connect_timeout(addr, Duration::from_millis(1500)) {
            Ok(stream) => {
                stream
                    .set_nodelay(true)
                    .map_err(|err| ControlError::Transport(err.to_string()))?;
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .map_err(|err| ControlError::Transport(err.to_string()))?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .map_err(|err| ControlError::Transport(err.to_string()))?;
                return Ok(stream);
            }
            Err(err) => last = ControlError::Transport(err.to_string()),
        }
    }
    Err(last)
}

/// Run an operation once. A transport failure leaves an uncertain result:
/// discard that connection and let the caller reconcile status before retrying.
pub(crate) fn call<T>(
    slot: &mut Option<ControlSession>,
    host: &str,
    port: u16,
    mut op: impl FnMut(&mut ControlSession) -> Result<T, ControlError>,
) -> Result<T, ControlError> {
    if slot.is_none() {
        *slot = Some(ControlSession::connect(host, port)?);
    }
    let result = op(slot.as_mut().expect("connected control session"));
    if matches!(result, Err(ControlError::Transport(_))) {
        *slot = None;
    }
    result
}

/// Retry a read-only GetInfo/Status request once after reconnecting. Never use
/// this for queue insertion, removal, playback controls, or mixed operations.
pub(crate) fn retry_read<T>(
    slot: &mut Option<ControlSession>,
    host: &str,
    port: u16,
    mut op: impl FnMut(&mut ControlSession) -> Result<T, ControlError>,
) -> Result<T, ControlError> {
    match call(slot, host, port, &mut op) {
        Err(ControlError::Transport(_)) => call(slot, host, port, op),
        result => result,
    }
}

pub(crate) fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn xml_unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        match &rest[..=end] {
            "&amp;" => out.push('&'),
            "&lt;" => out.push('<'),
            "&gt;" => out.push('>'),
            "&quot;" => out.push('"'),
            "&apos;" => out.push('\''),
            other => out.push_str(other),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn ensure_ok(doc: &str) -> Result<(), ControlError> {
    let Some(result) = attr(doc, "result") else {
        return Ok(());
    };
    if result.eq_ignore_ascii_case("error") || result.eq_ignore_ascii_case("unknown command") {
        let detail = element_text(doc).unwrap_or(result);
        return Err(ControlError::Rejected(detail));
    }
    Ok(())
}

fn element_text(doc: &str) -> Option<String> {
    let body = skip_declaration(doc.trim())?;
    let open_end = body.find('>')?;
    let bytes = body.as_bytes();
    let mut mark = open_end;
    while mark > 0 && bytes[mark - 1].is_ascii_whitespace() {
        mark -= 1;
    }
    if mark > 0 && bytes[mark - 1] == b'/' {
        return None;
    }
    let name = root_name(doc)?;
    let close = format!("</{name}>");
    let rest = &body[open_end + 1..];
    let end = rest.find(&close)?;
    let text = rest[..end].trim();
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(xml_unescape(text))
}

fn skip_declaration(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    if let Some(stripped) = rest.strip_prefix("<?") {
        let end = stripped.find("?>")?;
        rest = stripped[end + 2..].trim_start();
    }
    Some(rest)
}

fn root_name(doc: &str) -> Option<&str> {
    let body = skip_declaration(doc.trim_start())?;
    let rest = body.strip_prefix('<')?;
    if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
        return None;
    }
    let end = rest
        .find(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
        .unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then_some(name)
}

fn attr(doc: &str, name: &str) -> Option<String> {
    let body = skip_declaration(doc.trim())?;
    let bytes = body.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut i = 1usize;
    let mut quote: Option<u8> = None;
    let mut gt = None;
    while i < bytes.len() {
        let ch = bytes[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == b'"' || ch == b'\'' {
            quote = Some(ch);
        } else if ch == b'>' {
            gt = Some(i);
            break;
        }
        i += 1;
    }
    let gt = gt?;
    let tag = &body[1..gt];
    let tag_bytes = tag.as_bytes();
    let mut i = 0usize;
    while i < tag_bytes.len() && !tag_bytes[i].is_ascii_whitespace() && tag_bytes[i] != b'/' {
        i += 1;
    }
    while i < tag_bytes.len() {
        while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_bytes.len() || tag_bytes[i] == b'/' {
            break;
        }
        let key_start = i;
        while i < tag_bytes.len() && tag_bytes[i] != b'=' && !tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &tag[key_start..i];
        while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_bytes.len() || tag_bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < tag_bytes.len() && tag_bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag_bytes.len() {
            break;
        }
        let q = tag_bytes[i];
        if q != b'"' && q != b'\'' {
            break;
        }
        i += 1;
        let value_start = i;
        while i < tag_bytes.len() && tag_bytes[i] != q {
            i += 1;
        }
        let value = &tag[value_start..i];
        if i < tag_bytes.len() {
            i += 1;
        }
        if key == name {
            return Some(xml_unescape(value));
        }
    }
    None
}

/// Split one complete XML document off the front of `buf`.
///
/// A self-closing child (`<metadata .../>`) does not finish a parent element.
/// Leading whitespace is a keepalive and is not part of the document.
pub(crate) fn split_document(buf: &str) -> Option<(String, &str)> {
    let bytes = buf.as_bytes();
    let start = buf.find(|ch: char| !ch.is_whitespace())?;
    let mut i = start;
    if bytes[i..].starts_with(b"<?") {
        let rel = buf[i..].find("?>")?;
        i += rel + 2;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }
    if i >= bytes.len() || bytes[i] != b'<' {
        return None;
    }
    let name_start = i + 1;
    if name_start >= bytes.len() || bytes[name_start] == b'/' {
        return None;
    }
    let mut j = name_start;
    while j < bytes.len() && !bytes[j].is_ascii_whitespace() && bytes[j] != b'>' && bytes[j] != b'/'
    {
        j += 1;
    }
    if j == name_start {
        return None;
    }
    let name = &buf[name_start..j];
    let mut quote: Option<u8> = None;
    let mut gt = None;
    let mut k = j;
    while k < bytes.len() {
        let ch = bytes[k];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
        } else if ch == b'"' || ch == b'\'' {
            quote = Some(ch);
        } else if ch == b'>' {
            gt = Some(k);
            break;
        }
        k += 1;
    }
    let gt = gt?;
    let mut mark = gt;
    while mark > i && bytes[mark - 1].is_ascii_whitespace() {
        mark -= 1;
    }
    let self_closing = mark > i && bytes[mark - 1] == b'/';
    let end = if self_closing {
        gt + 1
    } else {
        let close = format!("</{name}>");
        let rest = &buf[gt + 1..];
        let rel = rest.find(&close)?;
        gt + 1 + rel + close.len()
    };
    Some((buf[start..end].to_string(), &buf[end..]))
}

pub(crate) fn pcm_bytes(frames: u64, channels: u32) -> Option<u32> {
    if channels == 0 || channels > 32 {
        return None;
    }
    let bytes = frames.checked_mul(u64::from(channels))?.checked_mul(4)?;
    u32::try_from(bytes)
        .ok()
        .filter(|bytes| *bytes <= MAX_WAV_DATA)
}

/// PCM payload size for `remain_secs` of S32LE. `None` when it would not fit
/// in a WAV data chunk (4 GiB).
pub(crate) fn bytes_from_duration(remain_secs: f64, rate: u32, channels: u32) -> Option<u32> {
    if !remain_secs.is_finite() || remain_secs < 0.0 || rate == 0 || channels == 0 {
        return None;
    }
    if remain_secs == 0.0 {
        return Some(0);
    }
    let frames = (remain_secs * f64::from(rate)).floor();
    if !frames.is_finite() || frames < 0.0 || frames > u64::MAX as f64 {
        return None;
    }
    pcm_bytes(frames as u64, channels)
}

fn wav_header(rate: u32, channels: u16, data_bytes: u32) -> [u8; 44] {
    debug_assert!(data_bytes <= MAX_WAV_DATA);
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    let block = u32::from(channels).saturating_mul(4);
    header[28..32].copy_from_slice(&rate.saturating_mul(block).to_le_bytes());
    header[32..34].copy_from_slice(&(block as u16).to_le_bytes());
    header[34..36].copy_from_slice(&32u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    header
}

/// PCM for one handoff. Range requests seek this file, so the track does not
/// have to sit in RAM. The file goes away with the feed.
struct PcmFile {
    file: std::fs::File,
    len: u64,
}

impl PcmFile {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("sone-hq-{}.pcm", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| format!("could not create HQPlayer PCM file: {err}"))?;
        // An anonymous file cannot be observed via its pathname or left behind
        // after a crash. Existing handles remain valid until the feed is dropped.
        std::fs::remove_file(&path)
            .map_err(|err| format!("could not unlink HQPlayer PCM file: {err}"))?;
        Ok(Self { file, len: 0 })
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) -> bool {
        if self.file.set_len(0).is_err() {
            return false;
        }
        self.len = 0;
        true
    }

    fn truncate(&mut self, len: u64) -> bool {
        let len = len.min(self.len);
        if self.file.set_len(len).is_err() {
            return false;
        }
        self.len = len;
        true
    }

    fn append(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.file.seek(SeekFrom::Start(self.len)).is_err() {
            return false;
        }
        if self.file.write_all(bytes).is_err() {
            return false;
        }
        self.len += bytes.len() as u64;
        true
    }

    fn read_at(&mut self, offset: u64, dst: &mut [u8]) -> usize {
        if offset >= self.len || dst.is_empty() {
            return 0;
        }
        if self.file.seek(SeekFrom::Start(offset)).is_err() {
            return 0;
        }
        let want = ((self.len - offset) as usize).min(dst.len());
        self.file.read(&mut dst[..want]).unwrap_or(0)
    }

    #[cfg(test)]
    fn read_all(&mut self) -> Vec<u8> {
        let mut out = vec![0u8; self.len as usize];
        let n = self.read_at(0, &mut out);
        out.truncate(n);
        out
    }
}

struct FeedInner {
    accepting: bool,
    capture: u64,
    cancelled: bool,
    finished: bool,
    failure: Option<String>,
    rate: Option<u32>,
    channels: Option<u32>,
    pcm: PcmFile,
    pending: Vec<u8>,
    /// Samples pulled before `begin_capture` (preroll, or the buffer that
    /// lands while a seek is flushing). Adopted into `pcm` at capture start
    /// so the first decoded buffer is not dropped.
    early: Vec<u8>,
    data_bytes: Option<u32>,
    header: Option<[u8; 44]>,
}

struct SharedFeed {
    id: uuid::Uuid,
    port: u16,
    owners: AtomicUsize,
    cancelled: AtomicBool,
    readers: AtomicUsize,
    pair: (Mutex<FeedInner>, Condvar),
    clients: Mutex<Vec<Arc<TcpStream>>>,
}

enum Fill {
    /// `n` bytes were written. `0` means the caller must wait.
    Ready(usize),
    Done,
    Failed(String),
}

/// Local HTTP WAV. One generation, one path: `/s/{id}.wav`.
pub(crate) struct WavFeed {
    shared: Arc<SharedFeed>,
}

impl Clone for WavFeed {
    fn clone(&self) -> Self {
        self.shared.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for WavFeed {
    fn drop(&mut self) {
        if self.shared.owners.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.cancel();
        }
    }
}

impl WavFeed {
    pub(crate) fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| err.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|err| err.to_string())?;
        let port = listener.local_addr().map_err(|err| err.to_string())?.port();
        let id = uuid::Uuid::new_v4();
        let pcm = PcmFile::create()?;
        let shared = Arc::new(SharedFeed {
            id,
            port,
            owners: AtomicUsize::new(1),
            cancelled: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            pair: (
                Mutex::new(FeedInner {
                    accepting: false,
                    capture: 0,
                    cancelled: false,
                    finished: false,
                    failure: None,
                    rate: None,
                    channels: None,
                    pcm,
                    pending: Vec::new(),
                    early: Vec::new(),
                    data_bytes: None,
                    header: None,
                }),
                Condvar::new(),
            ),
            clients: Mutex::new(Vec::new()),
        });
        let accept_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("hqplayer-wav".into())
            .spawn(move || accept_loop(listener, accept_shared))
            .map_err(|err| err.to_string())?;
        Ok(Self { shared })
    }

    pub(crate) fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/s/{}.wav",
            self.shared.port, self.shared.id
        )
    }

    pub(crate) fn reader_count(&self) -> usize {
        self.shared.readers.load(Ordering::SeqCst)
    }

    pub(crate) fn failure(&self) -> Option<String> {
        lock_feed(&self.shared).failure.clone()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst)
    }

    /// Non-zero while samples should be stored. `0` means drop them.
    pub(crate) fn capture_token(&self) -> u64 {
        let inner = lock_feed(&self.shared);
        if inner.accepting {
            inner.capture
        } else {
            0
        }
    }

    pub(crate) fn set_format(&self, rate: u32, channels: u32) {
        let mut inner = lock_feed(&self.shared);
        if rate == 0 || channels == 0 || channels > 32 || rate.checked_mul(channels * 4).is_none() {
            fail_feed(
                &mut inner,
                "HQPlayer PCM format cannot be represented as WAV",
            );
            self.shared.pair.1.notify_all();
            return;
        }
        if let Some(previous_rate) = inner.rate {
            if previous_rate != rate || inner.channels != Some(channels) {
                fail_feed(
                    &mut inner,
                    "HQPlayer PCM format changed inside one WAV stream",
                );
                self.shared.pair.1.notify_all();
            }
            return;
        }
        inner.rate = Some(rate);
        inner.channels = Some(channels);
        self.shared.pair.1.notify_all();
    }

    pub(crate) fn begin_capture(&self) {
        let mut inner = lock_feed(&self.shared);
        if inner.cancelled || inner.failure.is_some() {
            return;
        }
        if !inner.pcm.clear() {
            fail_feed(&mut inner, "could not clear the HQPlayer PCM file");
            self.shared.pair.1.notify_all();
            return;
        }
        inner.pending.clear();
        inner.capture = inner.capture.saturating_add(1);
        inner.accepting = true;
        fold_pending(&mut inner);
        self.shared.pair.1.notify_all();
    }

    /// Forget samples pulled before a seek. The next preroll fills `early`
    /// again, and `begin_capture` keeps only that.
    pub(crate) fn drop_early(&self) {
        let mut inner = lock_feed(&self.shared);
        inner.early.clear();
    }

    /// Store `bytes` when capture is open. Before it is, keep them so
    /// `begin_capture` can adopt the preroll instead of dropping it.
    pub(crate) fn offer(&self, token: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut inner = lock_feed(&self.shared);
            if inner.cancelled || inner.finished {
                return;
            }
            if !inner.accepting {
                if inner.early.len().saturating_add(bytes.len()) > MAX_PREROLL_BYTES {
                    fail_feed(&mut inner, "HQPlayer preroll exceeded its memory limit");
                    self.shared.pair.1.notify_all();
                    return;
                }
                inner.early.extend_from_slice(bytes);
                self.shared.pair.1.notify_all();
                return;
            }
            if token == 0 || inner.capture != token {
                return;
            }
        }
        self.push(token, bytes);
    }

    pub(crate) fn wait_format(&self, timeout: Duration) -> Result<(u32, u32), String> {
        let deadline = Instant::now() + timeout;
        let mut inner = lock_feed(&self.shared);
        loop {
            if let Some(error) = &inner.failure {
                return Err(error.clone());
            }
            if inner.cancelled {
                return Err("cancelled".into());
            }
            if let (Some(rate), Some(channels)) = (inner.rate, inner.channels) {
                return Ok((rate, channels));
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for S32LE audio".into());
            }
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(200));
            let (next, _) = self
                .shared
                .pair
                .1
                .wait_timeout(inner, slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = next;
        }
    }

    pub(crate) fn set_data_bytes(&self, bytes: u32) -> Result<(), String> {
        let mut inner = lock_feed(&self.shared);
        if let Some(error) = &inner.failure {
            return Err(error.clone());
        }
        if inner.cancelled {
            return Err("cancelled".into());
        }
        if bytes > MAX_WAV_DATA {
            fail_feed(&mut inner, "track is too long for a RIFF/WAV handoff");
            self.shared.pair.1.notify_all();
            return Err(inner.failure.clone().unwrap());
        }
        let rate = inner.rate.ok_or_else(|| "format unknown".to_string())?;
        let channels = inner.channels.ok_or_else(|| "format unknown".to_string())?;
        let frame = channels.saturating_mul(4);
        if frame == 0 {
            return Err("bad channel count".into());
        }
        let aligned = bytes / frame * frame;
        if let Some(previous) = inner.data_bytes {
            if inner.finished || previous == aligned {
                return Ok(());
            }
            return Err("cannot change the size of an active WAV stream".into());
        }
        inner.data_bytes = Some(aligned);
        inner.header = Some(wav_header(rate, channels as u16, aligned));
        if inner.pcm.len > u64::from(aligned) && !inner.pcm.truncate(u64::from(aligned)) {
            fail_feed(&mut inner, "could not size the HQPlayer PCM file");
            self.shared.pair.1.notify_all();
            return Err(inner.failure.clone().unwrap());
        }
        self.shared.pair.1.notify_all();
        Ok(())
    }

    pub(crate) fn push(&self, token: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut inner = lock_feed(&self.shared);
        if inner.cancelled || inner.finished || !inner.accepting || inner.capture != token {
            return;
        }
        let Some(channels) = inner.channels else {
            return;
        };
        if channels.saturating_mul(4) == 0 {
            return;
        }
        inner.pending.extend_from_slice(bytes);
        drain_whole_frames(&mut inner);
        self.shared.pair.1.notify_all();
    }

    pub(crate) fn finish(&self) {
        let mut inner = lock_feed(&self.shared);
        if inner.cancelled || inner.finished {
            self.shared.pair.1.notify_all();
            return;
        }
        fold_pending(&mut inner);
        if !inner.pending.is_empty() {
            fail_feed(&mut inner, "HQPlayer PCM ended with an incomplete frame");
        }
        inner.finished = true;
        if let (Some(expected), Some(channels)) = (inner.data_bytes, inner.channels) {
            // GStreamer's duration is truncated to a clock tick; tolerate at
            // most one frame, never turn a failed decode into a silent tail.
            if u64::from(expected).saturating_sub(inner.pcm.len) > u64::from(channels) * 4 {
                fail_feed(
                    &mut inner,
                    "decoded PCM ended before the advertised WAV length",
                );
                self.shared.pair.1.notify_all();
                return;
            }
        }
        if inner.data_bytes.is_none() {
            if let (Some(rate), Some(channels)) = (inner.rate, inner.channels) {
                let frame = u64::from(channels.saturating_mul(4));
                let aligned = inner.pcm.len.checked_div(frame).unwrap_or(0) * frame;
                if let Some(len) = u32::try_from(aligned)
                    .ok()
                    .filter(|len| *len <= MAX_WAV_DATA)
                {
                    if aligned < inner.pcm.len && !inner.pcm.truncate(aligned) {
                        fail_feed(&mut inner, "could not finalize the HQPlayer PCM file");
                        self.shared.pair.1.notify_all();
                        return;
                    }
                    inner.data_bytes = Some(len);
                    inner.header = Some(wav_header(rate, channels as u16, len));
                }
            }
        }
        self.shared.pair.1.notify_all();
    }

    pub(crate) fn wait_finished(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut inner = lock_feed(&self.shared);
        loop {
            if let Some(error) = &inner.failure {
                return Err(error.clone());
            }
            if inner.cancelled {
                return Err("cancelled".into());
            }
            if inner.finished && inner.header.is_some() {
                return Ok(());
            }
            if inner.finished {
                return Err(if inner.rate.is_none() {
                    "HQPlayer handoff produced no PCM".into()
                } else {
                    "track is too long for a WAV handoff".into()
                });
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for the track to decode".into());
            }
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(200));
            let (next, _) = self
                .shared
                .pair
                .1
                .wait_timeout(inner, slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = next;
        }
    }

    #[cfg(test)]
    fn stored_pcm(&self) -> Vec<u8> {
        lock_feed(&self.shared).pcm.read_all()
    }

    pub(crate) fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        {
            let mut inner = lock_feed(&self.shared);
            inner.cancelled = true;
            inner.accepting = false;
            inner.early.clear();
            inner.pending.clear();
            let _ = inner.pcm.clear();
            self.shared.pair.1.notify_all();
        }
        if let Ok(mut clients) = self.shared.clients.lock() {
            for sock in clients.drain(..) {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
    }
}

fn fold_pending(inner: &mut FeedInner) {
    let Some(channels) = inner.channels else {
        return;
    };
    if channels.saturating_mul(4) == 0 {
        return;
    }
    if !inner.early.is_empty() {
        inner.pending.append(&mut inner.early);
    }
    drain_whole_frames(inner);
}

/// Move whole frames from `pending` onto `pcm`. Bytes past the declared WAV
/// length are dropped. Returns false when `pending` holds no whole frame.
fn drain_whole_frames(inner: &mut FeedInner) -> bool {
    let Some(channels) = inner.channels else {
        return false;
    };
    let frame = channels as usize * 4;
    if frame == 0 {
        return false;
    }
    let whole = inner.pending.len() / frame * frame;
    if whole == 0 {
        return false;
    }
    let room = inner
        .data_bytes
        .map(|limit| (u64::from(limit)).saturating_sub(inner.pcm.len) as usize)
        .unwrap_or(whole);
    let add = whole.min(room);
    if inner.pcm.len.saturating_add(add as u64) > u64::from(MAX_WAV_DATA) {
        fail_feed(inner, "track is too long for a RIFF/WAV handoff");
        return false;
    }
    if add > 0 && !inner.pcm.append(&inner.pending[..add]) {
        fail_feed(
            inner,
            "could not write the HQPlayer PCM file (check available disk space)",
        );
        return false;
    }
    let tail = inner.pending.len() - whole;
    inner.pending.copy_within(whole.., 0);
    inner.pending.truncate(tail);
    true
}

fn fail_feed(inner: &mut FeedInner, message: &str) {
    if inner.failure.is_none() {
        inner.failure = Some(message.to_string());
    }
    inner.accepting = false;
    inner.finished = true;
    inner.pending.clear();
    inner.early.clear();
}

fn lock_feed(shared: &SharedFeed) -> std::sync::MutexGuard<'_, FeedInner> {
    shared
        .pair
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn accept_loop(listener: TcpListener, shared: Arc<SharedFeed>) {
    while !shared.cancelled.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, _)) => {
                sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
                sock.set_write_timeout(Some(Duration::from_secs(2))).ok();
                let sock = Arc::new(sock);
                {
                    let mut clients = shared
                        .clients
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if shared.cancelled.load(Ordering::SeqCst) || clients.len() >= MAX_HTTP_CLIENTS
                    {
                        let _ = sock.shutdown(Shutdown::Both);
                        continue;
                    }
                    clients.push(Arc::clone(&sock));
                }
                let client = Arc::clone(&sock);
                let worker_shared = Arc::clone(&shared);
                if std::thread::Builder::new()
                    .name("hqplayer-http".into())
                    .spawn(move || {
                        let _ = handle_client(&client, &worker_shared);
                        if let Ok(mut clients) = worker_shared.clients.lock() {
                            clients.retain(|open| !Arc::ptr_eq(open, &client));
                        }
                    })
                    .is_err()
                {
                    let _ = sock.shutdown(Shutdown::Both);
                    if let Ok(mut clients) = shared.clients.lock() {
                        clients.retain(|open| !Arc::ptr_eq(open, &sock));
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

fn handle_client(sock: &TcpStream, shared: &SharedFeed) -> std::io::Result<()> {
    let headers = match read_request(sock) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    let mut lines = headers.split("\r\n");
    let Some(request) = lines.next() else {
        return Ok(());
    };
    let mut parts = request.split_whitespace();
    let Some(method) = parts.next() else {
        return Ok(());
    };
    let Some(path) = parts.next() else {
        return Ok(());
    };
    if method != "GET" && method != "HEAD" {
        return write_status(sock, "405 Method Not Allowed", &[]);
    }
    let expected = format!("/s/{}.wav", shared.id);
    if path != expected {
        return write_status(sock, "404 Not Found", &[]);
    }
    let Some(data_bytes) = wait_header(shared) else {
        return Ok(());
    };
    let total = 44u64 + u64::from(data_bytes);
    let range = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });
    let span = match range.as_deref().map(|value| parse_range(value, total)) {
        Some(RangeHit::Partial { start, end }) => Some((start, end)),
        Some(RangeHit::Unsatisfiable) => {
            return write_status(
                sock,
                "416 Range Not Satisfiable",
                &[("Content-Range", format!("bytes */{total}"))],
            );
        }
        Some(RangeHit::Ignore) | None => None,
    };
    let (start, end) = span.unwrap_or((0, total.saturating_sub(1)));
    let len = end - start + 1;
    let status = if span.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut extra = vec![
        ("Content-Type", "audio/wav".to_string()),
        ("Content-Length", len.to_string()),
        ("Accept-Ranges", "bytes".to_string()),
        ("Cache-Control", "no-store".to_string()),
    ];
    if span.is_some() {
        extra.push(("Content-Range", format!("bytes {start}-{end}/{total}")));
    }
    write_head(sock, status, &extra)?;
    if method == "HEAD" {
        return Ok(());
    }
    shared.readers.fetch_add(1, Ordering::SeqCst);
    let result = write_body(shared, sock, start, len);
    shared.readers.fetch_sub(1, Ordering::SeqCst);
    result
}

enum RangeHit {
    Ignore,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn parse_range(value: &str, total: u64) -> RangeHit {
    if total == 0 {
        return RangeHit::Unsatisfiable;
    }
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeHit::Ignore;
    };
    if spec.contains(',') || spec.is_empty() {
        return RangeHit::Ignore;
    }
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return RangeHit::Ignore;
    };
    if start_s.is_empty() {
        return match end_s.parse::<u64>() {
            Ok(0) => RangeHit::Unsatisfiable,
            Ok(suffix) => RangeHit::Partial {
                start: total.saturating_sub(suffix),
                end: total - 1,
            },
            Err(_) => RangeHit::Ignore,
        };
    }
    let Ok(start) = start_s.parse::<u64>() else {
        return RangeHit::Ignore;
    };
    if start >= total {
        return RangeHit::Unsatisfiable;
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        match end_s.parse::<u64>() {
            Ok(end) => end,
            Err(_) => return RangeHit::Ignore,
        }
    };
    let end = end.min(total - 1);
    if end < start {
        return RangeHit::Unsatisfiable;
    }
    RangeHit::Partial { start, end }
}

fn wait_header(shared: &SharedFeed) -> Option<u32> {
    let deadline = Instant::now() + HTTP_WAIT_LIMIT;
    let mut inner = lock_feed(shared);
    loop {
        if inner.cancelled
            || inner.failure.is_some()
            || Instant::now() >= deadline
            || shared.cancelled.load(Ordering::SeqCst)
        {
            return None;
        }
        if let (Some(bytes), Some(_header)) = (inner.data_bytes, inner.header) {
            return Some(bytes);
        }
        let (next, _) = shared
            .pair
            .1
            .wait_timeout(inner, Duration::from_millis(200))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner = next;
    }
}

fn fill_available(inner: &mut FeedInner, offset: u64, dst: &mut [u8]) -> Fill {
    if let Some(error) = &inner.failure {
        return Fill::Failed(error.clone());
    }
    let (Some(data_bytes), Some(header)) = (inner.data_bytes, inner.header.as_ref()) else {
        return Fill::Ready(0);
    };
    let total = 44u64 + u64::from(data_bytes);
    if offset >= total || dst.is_empty() {
        return Fill::Done;
    }
    // Hold the RIFF header until the first PCM byte exists. HTTP already
    // advertised Content-Length. A finished feed, or a zero-length data
    // chunk, has nothing further to wait for.
    if offset < 44 && inner.pcm.is_empty() && !inner.finished && data_bytes != 0 {
        return Fill::Ready(0);
    }
    let mut filled = 0usize;
    while filled < dst.len() && offset + (filled as u64) < total {
        let pos = offset + filled as u64;
        if pos < 44 {
            let from = pos as usize;
            let n = (44 - from).min(dst.len() - filled);
            dst[filled..filled + n].copy_from_slice(&header[from..from + n]);
            filled += n;
            continue;
        }
        let pcm_off = (pos - 44) as usize;
        let limit = data_bytes as usize;
        if pcm_off >= limit {
            break;
        }
        if (pcm_off as u64) < inner.pcm.len {
            let n = ((inner.pcm.len as usize) - pcm_off)
                .min(dst.len() - filled)
                .min(limit - pcm_off);
            let got = inner
                .pcm
                .read_at(pcm_off as u64, &mut dst[filled..filled + n]);
            if got == 0 {
                fail_feed(inner, "could not read the HQPlayer PCM file");
                return Fill::Failed(inner.failure.clone().unwrap());
            }
            filled += got;
            continue;
        }
        if inner.finished {
            let n = (limit - pcm_off).min(dst.len() - filled);
            dst[filled..filled + n].fill(0);
            filled += n;
            continue;
        }
        break;
    }
    if filled > 0 {
        Fill::Ready(filled)
    } else if inner.finished {
        Fill::Done
    } else {
        Fill::Ready(0)
    }
}

fn write_body(
    shared: &SharedFeed,
    mut sock: &TcpStream,
    start: u64,
    len: u64,
) -> std::io::Result<()> {
    let end = start + len;
    let mut offset = start;
    let mut buf = vec![0u8; 64 * 1024];
    while offset < end {
        if shared.cancelled.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "cancelled",
            ));
        }
        let want = ((end - offset) as usize).min(buf.len());
        let n = wait_fill(shared, offset, &mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "wav ended early",
            ));
        }
        sock.write_all(&buf[..n])?;
        offset += n as u64;
    }
    Ok(())
}

fn wait_fill(shared: &SharedFeed, offset: u64, dst: &mut [u8]) -> std::io::Result<usize> {
    let deadline = Instant::now() + HTTP_WAIT_LIMIT;
    let mut inner = lock_feed(shared);
    loop {
        if inner.cancelled || shared.cancelled.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "cancelled",
            ));
        }
        match fill_available(&mut inner, offset, dst) {
            Fill::Ready(0) => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "waiting for decoded PCM timed out",
                    ));
                }
                let (next, _) = shared
                    .pair
                    .1
                    .wait_timeout(inner, Duration::from_millis(200))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner = next;
            }
            Fill::Ready(n) => return Ok(n),
            Fill::Done => return Ok(0),
            Fill::Failed(message) => return Err(std::io::Error::other(message)),
        }
    }
}

fn read_request(mut sock: &TcpStream) -> Result<String, ()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while buf.len() < 8192 && Instant::now() < deadline {
        match sock.read(&mut tmp) {
            Ok(0) => return Err(()),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    return String::from_utf8(buf).map_err(|_| ());
                }
            }
            Err(_) => return Err(()),
        }
    }
    Err(())
}

fn write_status(sock: &TcpStream, status: &str, extra: &[(&str, String)]) -> std::io::Result<()> {
    let mut headers = vec![("Content-Length", "0".to_string())];
    headers.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    write_head(sock, status, &headers)
}

fn write_head(
    mut sock: &TcpStream,
    status: &str,
    headers: &[(&str, String)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_lays_out_riff_and_pcm() {
        let header = wav_header(48_000, 2, 32);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 68);
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(&header[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(header[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(header[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(header[22..24].try_into().unwrap()), 2);
        assert_eq!(
            u32::from_le_bytes(header[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes(header[28..32].try_into().unwrap()),
            48_000 * 8
        );
        assert_eq!(u16::from_le_bytes(header[32..34].try_into().unwrap()), 8);
        assert_eq!(u16::from_le_bytes(header[34..36].try_into().unwrap()), 32);
        assert_eq!(&header[36..40], b"data");
        assert_eq!(u32::from_le_bytes(header[40..44].try_into().unwrap()), 32);
    }

    #[test]
    fn duration_that_exceeds_a_wav_is_rejected() {
        assert_eq!(bytes_from_duration(10.0, 48_000, 2), Some(48_000 * 10 * 8));
        assert!(bytes_from_duration(800.0, 768_000, 2).is_none());
    }

    #[test]
    fn xml_escapes_and_a_child_tag_does_not_finish_the_parent() {
        assert_eq!(xml_escape(r#"a&b<c>"'"#), "a&amp;b&lt;c&gt;&quot;&apos;");
        let doc = "<?xml version=\"1.0\"?><Status state=\"2\" position=\"1.5\" length=\"10\"><metadata uri=\"a&amp;b\"/></Status>\n<next/>";
        let (one, rest) = split_document(doc).unwrap();
        assert!(one.contains("</Status>"));
        assert!(rest.trim_start().starts_with("<next/>"));
        assert_eq!(attr(&one, "state").as_deref(), Some("2"));
        assert_eq!(attr(&one, "position").as_deref(), Some("1.5"));
        assert!(attr(&one, "uri").is_none());
        assert_eq!(xml_unescape("a&amp;b"), "a&b");
        assert!(split_document("\n\n").is_none());
        let (closed, rest) = split_document("<GetInfo product=\"Desk\"/>").unwrap();
        assert!(rest.is_empty());
        assert_eq!(attr(&closed, "product").as_deref(), Some("Desk"));
    }

    #[test]
    fn control_roundtrip_stays_on_one_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            sock.set_nodelay(true).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            for _ in 0..2 {
                loop {
                    let n = sock.read(&mut tmp).unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8(buf.clone()).unwrap();
                    if let Some((doc, rest)) = split_document(&text) {
                        seen_thread.lock().unwrap().push(doc.clone());
                        let reply = if doc.contains("GetInfo") {
                            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><GetInfo product=\"Signalyst HQPlayer Desktop\" engine=\"5.17.1\" platform=\"Linux\" version=\"5\"/>\n"
                        } else if doc.contains("PlayNextURI") {
                            "<PlayNextURI result=\"OK\"/>\n"
                        } else {
                            "<Nope result=\"Error\">Unknown command</Nope>\n"
                        };
                        sock.write_all(reply.as_bytes()).unwrap();
                        buf = rest.as_bytes().to_vec();
                        break;
                    }
                }
            }
        });

        let mut session = ControlSession::connect("127.0.0.1", port).unwrap();
        let info = session.get_info().unwrap();
        assert_eq!(info.product, "Signalyst HQPlayer Desktop");
        assert_eq!(info.engine, "5.17.1");
        session.play_next_uri("http://127.0.0.1:9/s/1.wav").unwrap();
        let docs = seen.lock().unwrap().clone();
        assert_eq!(docs.len(), 2);
        assert!(docs[1].contains("<PlayNextURI value=\"http://127.0.0.1:9/s/1.wav\"/>"));
        server.join().unwrap();
    }

    #[test]
    fn http_get_blocks_until_the_pcm_exists() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(32).unwrap();
        let url = feed.url();
        let mut sock =
            TcpStream::connect(url.trim_start_matches("http://").split('/').next().unwrap())
                .unwrap();
        sock.set_nodelay(true).ok();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let path = url.splitn(4, '/').nth(3).unwrap();
        sock.write_all(format!("GET /{path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .unwrap();

        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let headers_done = loop {
            let n = sock.read(&mut tmp).unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..headers_done]).to_string();
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(head.contains("Content-Length: 76"));
        assert!(head.contains("Content-Type: audio/wav"));
        assert_eq!(buf.len(), headers_done, "body must wait for PCM");

        sock.set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        match sock.read(&mut tmp) {
            Err(err) => assert!(
                err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::WouldBlock,
                "{err}"
            ),
            Ok(n) => panic!("wav body arrived before PCM was pushed ({n} bytes)"),
        }

        let mut pcm = vec![0x11u8; 32];
        pcm[0] = 0x7f;
        feed.begin_capture();
        let token = feed.capture_token();
        feed.push(token, &pcm);

        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        while buf.len() < headers_done + 76 {
            let n = sock.read(&mut tmp).unwrap();
            assert!(n > 0);
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = &buf[headers_done..];
        assert_eq!(&body[0..4], b"RIFF");
        assert_eq!(body.len(), 76);
        assert_eq!(&body[44..], pcm.as_slice());
        feed.cancel();
    }

    #[test]
    fn preroll_before_capture_is_kept_and_a_seek_can_drop_it() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.offer(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(feed.stored_pcm().is_empty());
        feed.drop_early();
        feed.offer(0, &[9, 9, 9, 9, 8, 8, 8, 8]);
        feed.begin_capture();
        assert_eq!(feed.stored_pcm(), vec![9, 9, 9, 9, 8, 8, 8, 8]);
        let token = feed.capture_token();
        feed.offer(0, &[1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(feed.stored_pcm().len(), 8);
        feed.offer(token, &[4, 4, 4, 4, 4, 4, 4, 4]);
        assert_eq!(feed.stored_pcm().len(), 16);
        feed.cancel();
    }

    #[test]
    fn other_paths_are_not_files() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(0).unwrap();
        let url = feed.url();
        let host = url.trim_start_matches("http://");
        let host = host.split('/').next().unwrap();
        let mut sock = TcpStream::connect(host).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        sock.write_all(b"GET /etc/passwd HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 512];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 404"));
        assert!(!text.contains("root:"));
        feed.cancel();
    }

    #[test]
    fn dial_falls_through_a_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let good = listener.local_addr().unwrap();
        let closed_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = closed_listener.local_addr().unwrap();
        drop(closed_listener);
        let server = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let stream = dial_addrs(&[closed, good]).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), good);
        drop(stream);
        server.join().unwrap();
    }

    fn receive_control(sock: &mut TcpStream) -> String {
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut pending = Vec::new();
        loop {
            let mut buf = [0; 1024];
            let len = sock.read(&mut buf).unwrap();
            assert_ne!(len, 0);
            pending.extend_from_slice(&buf[..len]);
            if let Some((doc, _)) = split_document(std::str::from_utf8(&pending).unwrap()) {
                return doc;
            }
        }
    }

    #[test]
    fn uncertain_mutation_is_never_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            assert!(receive_control(&mut first).contains("PlayNextURI"));
            drop(first); // The mutation happened, but its acknowledgement was lost.
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut duplicate, _)) => {
                        receive_control(&mut duplicate);
                        duplicate
                            .write_all(b"<PlayNextURI result=\"OK\"/>")
                            .unwrap();
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            false
        });
        let mut session = None;
        assert!(matches!(
            call(&mut session, "127.0.0.1", port, |s| s
                .play_next_uri("http://127.0.0.1/a.wav")),
            Err(ControlError::Transport(_))
        ));
        assert!(session.is_none());
        assert!(
            !server.join().unwrap(),
            "queue insertion must not be duplicated"
        );
    }

    #[test]
    fn read_only_request_reconnects_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            assert!(receive_control(&mut first).contains("GetInfo"));
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            assert!(receive_control(&mut second).contains("GetInfo"));
            second.write_all(b"<GetInfo product=\"Desktop\"/>").unwrap();
        });
        let mut session = None;
        assert_eq!(
            retry_read(&mut session, "127.0.0.1", port, ControlSession::get_info)
                .unwrap()
                .product,
            "Desktop"
        );
        server.join().unwrap();
    }

    #[test]
    fn control_rejects_nonlocal_hosts_without_resolving_them() {
        for host in [
            "example.com",
            "192.168.1.1",
            "0.0.0.0",
            "::",
            "::ffff:192.168.1.2",
        ] {
            assert!(matches!(
                ControlSession::connect(host, DEFAULT_PORT),
                Err(ControlError::Rejected(_))
            ));
        }
        assert_eq!(loopback_addrs("localhost", DEFAULT_PORT).unwrap().len(), 2);
        assert!(loopback_addrs("[::1]", DEFAULT_PORT).is_ok());
        assert!(loopback_addrs("127.0.0.1", 0).is_err());
    }

    #[test]
    fn changing_format_or_finishing_an_incomplete_frame_is_an_error() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_format(96_000, 2);
        assert!(feed.failure().unwrap().contains("format changed"));
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.begin_capture();
        feed.push(feed.capture_token(), &[0; 7]);
        feed.finish();
        assert!(feed.failure().unwrap().contains("incomplete frame"));
    }

    #[test]
    fn completed_decode_keeps_its_exact_size_instead_of_a_duration_estimate() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.begin_capture();
        feed.push(feed.capture_token(), &[0; 16]);
        feed.finish();
        feed.set_data_bytes(24).unwrap();
        assert_eq!(lock_feed(&feed.shared).data_bytes, Some(16));
    }

    #[test]
    fn riff_payload_limit_includes_the_header() {
        let frames = u64::from(MAX_WAV_DATA) / 8;
        assert_eq!(pcm_bytes(frames, 2), Some((frames * 8) as u32));
        assert!(pcm_bytes(frames + 1, 2).is_none());
        assert!(pcm_bytes(1, 0).is_none());
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        assert!(feed.set_data_bytes(u32::MAX).is_err());
        assert!(feed.failure().unwrap().contains("RIFF"));
    }

    #[test]
    fn pcm_file_is_private_and_unlinked_immediately() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let pcm = PcmFile::create().unwrap();
        let metadata = pcm.file.metadata().unwrap();
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        assert_eq!(metadata.nlink(), 0);
    }

    #[test]
    fn disk_write_failure_is_reported_and_wakes_waiters() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(8).unwrap();
        feed.begin_capture();
        lock_feed(&feed.shared).pcm.file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/full")
            .unwrap();
        feed.push(feed.capture_token(), &[0; 8]);
        assert!(feed.failure().unwrap().contains("write"));
        assert!(feed.wait_finished(Duration::from_millis(1)).is_err());
        assert!(wait_fill(&feed.shared, 0, &mut [0; 8]).is_err());
    }

    #[test]
    fn finished_short_decode_is_not_replaced_by_a_silent_tail() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(80).unwrap();
        feed.begin_capture();
        feed.push(feed.capture_token(), &[0; 8]);
        feed.finish();
        assert!(feed.failure().unwrap().contains("advertised WAV length"));
    }

    fn open_http(feed: &WavFeed, method: &str, headers: &str) -> TcpStream {
        let mut sock = TcpStream::connect(("127.0.0.1", feed.shared.port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        sock.write_all(
            format!(
                "{method} /s/{}.wav HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}\r\n",
                feed.shared.id
            )
            .as_bytes(),
        )
        .unwrap();
        sock
    }

    #[test]
    fn ranges_and_head_have_consistent_lengths() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(16).unwrap();
        feed.begin_capture();
        feed.push(
            feed.capture_token(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        );
        feed.finish();
        for (method, range, status, body) in [
            ("GET", "bytes=44-47", "206", vec![1, 2, 3, 4]),
            ("GET", "bytes=-4", "206", vec![13, 14, 15, 16]),
            ("HEAD", "bytes=44-47", "206", vec![]),
            ("GET", "bytes=60-", "416", vec![]),
        ] {
            let mut response = Vec::new();
            open_http(&feed, method, &format!("Range: {range}\r\n"))
                .read_to_end(&mut response)
                .unwrap();
            let end = response
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            assert!(String::from_utf8_lossy(&response[..end])
                .starts_with(&format!("HTTP/1.1 {status}")));
            assert_eq!(&response[end..], body);
        }
    }

    #[test]
    fn cancellation_releases_blocked_readers_and_pcm_storage() {
        let feed = WavFeed::bind().unwrap();
        feed.set_format(48_000, 2);
        feed.set_data_bytes(16).unwrap();
        feed.begin_capture();
        feed.push(feed.capture_token(), &[0; 8]);
        let cloned = feed.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            done_tx
                .send(wait_fill(&cloned.shared, 52, &mut [0; 8]).is_err())
                .unwrap();
        });
        feed.cancel();
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        reader.join().unwrap();
        assert!(lock_feed(&feed.shared).pcm.is_empty());
        assert_eq!(feed.capture_token(), 0);
    }

    #[test]
    fn http_connections_are_bounded_and_last_owner_cancels_them() {
        let feed = WavFeed::bind().unwrap();
        let shared = Arc::clone(&feed.shared);
        let clients: Vec<_> = (0..MAX_HTTP_CLIENTS + 2)
            .map(|_| TcpStream::connect(("127.0.0.1", feed.shared.port)).unwrap())
            .collect();
        let deadline = Instant::now() + Duration::from_secs(1);
        while shared.clients.lock().unwrap().len() < MAX_HTTP_CLIENTS && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(shared.clients.lock().unwrap().len(), MAX_HTTP_CLIENTS);
        drop(feed);
        assert!(shared.cancelled.load(Ordering::SeqCst));
        assert!(shared.clients.lock().unwrap().is_empty());
        drop(clients);
    }
}
