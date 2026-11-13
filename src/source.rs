//! Data sources: file, Unix socket and HTTP (`/stream`) feeds of
//! `llm-visuals.stats/1` snapshots.
//!
//! Each [`Source`] owns one background thread (std threads, blocking I/O,
//! no async runtime). The UI thread calls [`Source::feed`] to get a cheap
//! copy of the current state; staleness is computed lazily there, so it is
//! correct even when the worker is blocked in a read.

use crate::model::{Snapshot, SCHEMA};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Nothing new for this long while the link is up: the feed is stale.
pub const STALE_AFTER: Duration = Duration::from_secs(2);

const FILE_POLL: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const MAX_LINE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    File(PathBuf),
    Unix(PathBuf),
    Http { url: String, token: Option<String> },
}

impl SourceSpec {
    /// `/abs/path.json` or `file:///...` -> File; `unix:///p.sock` -> Unix;
    /// `http://host:port[/path]` -> Http (path defaults to `/stream`).
    pub fn parse(s: &str, token: Option<String>) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty source".into());
        }
        if s.starts_with("https://") {
            return Err(format!(
                "{s}: https:// is not supported (no TLS); use ssh -L or a VPN and an http:// URL"
            ));
        }
        if let Some(rest) = s.strip_prefix("http://") {
            let (authority, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            if authority.is_empty() || authority.contains('@') {
                return Err(format!("{s}: invalid http URL (expected http://host[:port][/path])"));
            }
            split_authority(authority).map_err(|e| format!("{s}: {e}"))?;
            let path = if path.is_empty() { "/stream" } else { path };
            return Ok(SourceSpec::Http {
                url: format!("http://{authority}{path}"),
                token,
            });
        }
        if let Some(p) = s.strip_prefix("unix://") {
            if !p.starts_with('/') {
                return Err(format!("{s}: unix source needs an absolute path (unix:///path.sock)"));
            }
            return Ok(SourceSpec::Unix(PathBuf::from(p)));
        }
        if let Some(p) = s.strip_prefix("file://") {
            if !p.starts_with('/') {
                return Err(format!("{s}: file source needs an absolute path (file:///path.json)"));
            }
            return Ok(SourceSpec::File(PathBuf::from(p)));
        }
        if s.contains("://") {
            return Err(format!("{s}: unsupported source scheme"));
        }
        Ok(SourceSpec::File(expand_home(s)))
    }

    /// Short human description, for logs and status lines.
    pub fn describe(&self) -> String {
        match self {
            SourceSpec::File(p) => format!("file {}", p.display()),
            SourceSpec::Unix(p) => format!("unix {}", p.display()),
            SourceSpec::Http { url, .. } => url.clone(),
        }
    }
}

/// Expand a leading `~` / `~/` with `$HOME`.
pub(crate) fn expand_home(s: &str) -> PathBuf {
    if s == "~" || s.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            let rest = s[1..].trim_start_matches('/');
            if !rest.is_empty() {
                p.push(rest);
            }
            return p;
        }
    }
    PathBuf::from(s)
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkStatus {
    Connecting,
    Live,
    Stale,
    Down(String),
}

#[derive(Clone)]
pub struct Feed {
    /// Last good snapshot (kept while Stale/Down for context).
    pub snapshot: Option<Arc<Snapshot>>,
    /// When it arrived locally.
    pub received: Option<Instant>,
    pub status: LinkStatus,
    /// Increments on every new snapshot.
    pub generation: u64,
}

impl Feed {
    pub fn is_live(&self) -> bool {
        self.status == LinkStatus::Live
    }
}

#[derive(Clone, PartialEq)]
enum Raw {
    Connecting,
    Live,
    Down(String),
}

struct Inner {
    snapshot: Option<Arc<Snapshot>>,
    received: Option<Instant>,
    raw: Raw,
    generation: u64,
}

type Shared = Arc<Mutex<Inner>>;

fn lock(s: &Shared) -> MutexGuard<'_, Inner> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Source {
    spec: SourceSpec,
    shared: Shared,
    stop: Arc<AtomicBool>,
    stale_after: Duration,
}

impl Source {
    pub fn spawn(spec: SourceSpec) -> Source {
        Self::spawn_with(spec, STALE_AFTER)
    }

    pub(crate) fn spawn_with(spec: SourceSpec, stale_after: Duration) -> Source {
        let shared: Shared = Arc::new(Mutex::new(Inner {
            snapshot: None,
            received: None,
            raw: Raw::Connecting,
            generation: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let w = Worker {
            shared: shared.clone(),
            stop: stop.clone(),
        };
        let wspec = spec.clone();
        let spawned = std::thread::Builder::new()
            .name("llmv-source".into())
            .spawn(move || w.run(wspec));
        if let Err(e) = spawned {
            w_set_down(&shared, format!("cannot start thread: {e}"));
        }
        Source {
            spec,
            shared,
            stop,
            stale_after,
        }
    }

    pub fn feed(&self) -> Feed {
        let g = lock(&self.shared);
        let status = match &g.raw {
            Raw::Connecting => LinkStatus::Connecting,
            Raw::Down(r) => LinkStatus::Down(r.clone()),
            Raw::Live => match g.received {
                Some(t) if t.elapsed() < self.stale_after => LinkStatus::Live,
                _ => LinkStatus::Stale,
            },
        };
        Feed {
            snapshot: g.snapshot.clone(),
            received: g.received,
            status,
            generation: g.generation,
        }
    }

    pub fn spec(&self) -> &SourceSpec {
        &self.spec
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn w_set_down(shared: &Shared, reason: String) {
    lock(shared).raw = Raw::Down(reason);
}

struct Worker {
    shared: Shared,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Sleep in small steps so a dropped Source ends its thread quickly.
    fn sleep(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.stopped() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    fn set_down(&self, reason: String) {
        w_set_down(&self.shared, reason);
    }

    /// Parse one JSON document and publish it. Returns true when a snapshot
    /// was accepted.
    fn ingest(&self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        match Snapshot::parse(text) {
            Ok(snap) => {
                let mut g = lock(&self.shared);
                if let Some(s) = snap.schema.as_deref() {
                    if s != SCHEMA {
                        g.raw = Raw::Down(format!("unsupported schema {s}"));
                        return false;
                    }
                }
                g.snapshot = Some(Arc::new(snap));
                g.received = Some(Instant::now());
                g.generation += 1;
                g.raw = Raw::Live;
                true
            }
            Err(_) => {
                let mut g = lock(&self.shared);
                if g.snapshot.is_none() {
                    g.raw = Raw::Down("bad JSON".into());
                }
                false
            }
        }
    }

    fn run(self, spec: SourceSpec) {
        match spec {
            SourceSpec::File(p) => self.run_file(&p),
            SourceSpec::Unix(p) => self.run_reconnecting(|w| w.session_unix(&p)),
            SourceSpec::Http { url, token } => {
                self.run_reconnecting(|w| w.session_http(&url, token.as_deref()))
            }
        }
    }

    fn run_reconnecting(&self, session: impl Fn(&Worker) -> Result<bool, String>) {
        let mut backoff = BACKOFF_MIN;
        while !self.stopped() {
            match session(self) {
                Ok(got) => {
                    if got {
                        backoff = BACKOFF_MIN;
                    }
                }
                Err(reason) => {
                    self.set_down(reason);
                }
            }
            self.sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    // ---- file ----

    fn run_file(&self, path: &Path) {
        let mut last: Option<(i64, i64, u64, u64)> = None;
        while !self.stopped() {
            match std::fs::metadata(path) {
                Err(e) => {
                    last = None;
                    self.set_down(io_reason(&e));
                }
                Ok(md) => {
                    let key = (md.mtime(), md.mtime_nsec(), md.len(), md.ino());
                    if last != Some(key) {
                        match std::fs::read_to_string(path) {
                            Ok(text) => {
                                last = Some(key);
                                let ok = self.ingest(&text)
                                    || text
                                        .lines()
                                        .rev()
                                        .find(|l| !l.trim().is_empty())
                                        .is_some_and(|l| self.ingest(l));
                                let _ = ok;
                            }
                            Err(e) => {
                                last = None;
                                self.set_down(io_reason(&e));
                            }
                        }
                    }
                }
            }
            self.sleep(FILE_POLL);
        }
    }

    // ---- unix ----

    fn session_unix(&self, path: &Path) -> Result<bool, String> {
        let stream = UnixStream::connect(path).map_err(|e| io_reason(&e))?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| io_reason(&e))?;
        let mut r = BufReader::new(stream);
        self.read_lines(&mut r)
    }

    // ---- http ----

    fn session_http(&self, url: &str, token: Option<&str>) -> Result<bool, String> {
        let rest = url.strip_prefix("http://").ok_or("bad URL")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = split_authority(authority)?;
        let addrs = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|_| "cannot resolve host".to_string())?;
        let mut last_err = "cannot resolve host".to_string();
        let mut stream = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = io_reason(&e),
            }
        }
        let mut stream = stream.ok_or(last_err)?;
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| io_reason(&e))?;
        stream
            .set_write_timeout(Some(READ_TIMEOUT))
            .map_err(|e| io_reason(&e))?;
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/x-ndjson\r\nConnection: close\r\nUser-Agent: llm-wayvisuals\r\n"
        );
        if let Some(t) = token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).map_err(|e| io_reason(&e))?;

        let mut r = BufReader::new(stream);
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| io_reason(&e))?;
        let code: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or("bad HTTP response")?;
        let mut chunked = false;
        loop {
            line.clear();
            let n = r.read_line(&mut line).map_err(|e| io_reason(&e))?;
            if n == 0 {
                return Err("bad HTTP response".into());
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                if k.trim().eq_ignore_ascii_case("transfer-encoding")
                    && v.to_ascii_lowercase().contains("chunked")
                {
                    chunked = true;
                }
            }
        }
        if code != 200 {
            return Err(match code {
                401 => "401 unauthorized".into(),
                403 => "403 forbidden".into(),
                404 => "404 not found".into(),
                503 => "503 unavailable".into(),
                c => format!("HTTP {c}"),
            });
        }
        if chunked {
            let mut body = BufReader::new(Chunked::new(r));
            self.read_lines(&mut body)
        } else {
            self.read_lines(&mut r)
        }
    }

    /// Read NDJSON lines until EOF / error / stop. `Ok(got_data)` on a clean
    /// end of stream; `Err(reason)` otherwise.
    fn read_lines<R: BufRead>(&self, r: &mut R) -> Result<bool, String> {
        let mut got = false;
        let mut buf = Vec::new();
        while !self.stopped() {
            buf.clear();
            let n = r
                .by_ref()
                .take(MAX_LINE)
                .read_until(b'\n', &mut buf)
                .map_err(|e| io_reason(&e))?;
            if n == 0 {
                self.set_down("stream ended".into());
                return Ok(got);
            }
            if buf.last() != Some(&b'\n') {
                if n as u64 >= MAX_LINE {
                    return Err("line too long".into());
                }
                // Partial last line: the stream ends right after.
                self.set_down("stream ended".into());
                return Ok(got);
            }
            if let Ok(s) = std::str::from_utf8(&buf) {
                if self.ingest(s) {
                    got = true;
                }
            }
        }
        Ok(got)
    }
}

fn io_reason(e: &io::Error) -> String {
    use io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => "connection refused".into(),
        NotFound => "no such file".into(),
        PermissionDenied => "permission denied".into(),
        TimedOut | WouldBlock => "timeout".into(),
        ConnectionReset | ConnectionAborted | BrokenPipe => "connection lost".into(),
        UnexpectedEof => "stream ended".into(),
        _ => {
            let mut s = e.to_string();
            s.truncate(60);
            s
        }
    }
}

/// Split `host[:port]` / `[v6]:port` (default port 80).
fn split_authority(a: &str) -> Result<(String, u16), String> {
    let (host, port) = if let Some(rest) = a.strip_prefix('[') {
        let end = rest.find(']').ok_or("bad IPv6 address")?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p),
            None if tail.is_empty() => None,
            None => return Err("bad host".into()),
        };
        (host, port)
    } else {
        match a.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (a, None),
        }
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    let port = match port {
        Some(p) => p.parse::<u16>().map_err(|_| format!("bad port {p:?}"))?,
        None => 80,
    };
    Ok((host.to_string(), port))
}

/// Minimal HTTP/1.1 chunked transfer decoder.
struct Chunked<R: BufRead> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    fn new(inner: R) -> Self {
        Chunked {
            inner,
            remaining: 0,
            done: false,
        }
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        let mut line = String::new();
        if self.inner.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let size = line.trim().split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            self.done = true;
        }
        self.remaining = size;
        Ok(())
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.next_chunk()?;
            if self.done {
                return Ok(0);
            }
        }
        let want = out.len().min(self.remaining as usize);
        let n = self.inner.read(&mut out[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= n as u64;
        if self.remaining == 0 {
            // Chunk data is followed by CRLF.
            let mut crlf = String::new();
            self.inner.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("llmvw-src-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn line(seq: u64) -> String {
        format!(
            r#"{{"schema":"llm-visuals.stats/1","seq":{seq},"models":[{{"pid":1,"name":"m","decode_tps":{seq}.5}}],"gpus":[]}}"#
        )
    }

    fn wait_for(src: &Source, what: &str, f: impl Fn(&Feed) -> bool) -> Feed {
        let end = Instant::now() + Duration::from_secs(6);
        loop {
            let feed = src.feed();
            if f(&feed) {
                return feed;
            }
            assert!(Instant::now() < end, "timeout waiting for {what}: {:?}", feed.status);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn write_atomic(dir: &Path, name: &str, text: &str) {
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, dir.join(name)).unwrap();
    }

    #[test]
    fn parse_specs() {
        assert_eq!(
            SourceSpec::parse("/run/x.json", None).unwrap(),
            SourceSpec::File("/run/x.json".into())
        );
        assert_eq!(
            SourceSpec::parse("file:///run/x.json", None).unwrap(),
            SourceSpec::File("/run/x.json".into())
        );
        assert_eq!(
            SourceSpec::parse("unix:///run/x.sock", None).unwrap(),
            SourceSpec::Unix("/run/x.sock".into())
        );
        assert_eq!(
            SourceSpec::parse("http://gpubox1.example.lan:9464", Some("t".into())).unwrap(),
            SourceSpec::Http {
                url: "http://gpubox1.example.lan:9464/stream".into(),
                token: Some("t".into())
            }
        );
        assert_eq!(
            SourceSpec::parse("http://[::1]:9464/stats", None).unwrap(),
            SourceSpec::Http {
                url: "http://[::1]:9464/stats".into(),
                token: None
            }
        );
        let e = SourceSpec::parse("https://h:1", None).unwrap_err();
        assert!(e.contains("ssh -L"), "{e}");
        assert!(SourceSpec::parse("", None).is_err());
        assert!(SourceSpec::parse("ftp://x", None).is_err());
        assert!(SourceSpec::parse("unix://rel.sock", None).is_err());
        assert!(SourceSpec::parse("http://host:notaport", None).is_err());
        assert!(SourceSpec::parse("http://", None).is_err());
    }

    #[test]
    fn file_live_stale_down() {
        let dir = tmpdir("file");
        let path = dir.join("a.json");
        let src = Source::spawn_with(SourceSpec::File(path.clone()), Duration::from_millis(600));
        wait_for(&src, "down", |f| matches!(f.status, LinkStatus::Down(_)));
        write_atomic(&dir, "a.json", &line(1));
        let f1 = wait_for(&src, "live", |f| f.is_live());
        assert_eq!(f1.snapshot.as_ref().unwrap().seq, Some(1));
        write_atomic(&dir, "a.json", &line(2));
        let f2 = wait_for(&src, "gen+1", |f| f.generation > f1.generation);
        assert_eq!(f2.snapshot.as_ref().unwrap().seq, Some(2));
        // no more updates -> stale, snapshot kept
        let f3 = wait_for(&src, "stale", |f| f.status == LinkStatus::Stale);
        assert!(f3.snapshot.is_some());
        assert_eq!(f3.generation, f2.generation);
        // same content rewritten (new inode) -> live again
        write_atomic(&dir, "a.json", &line(2));
        wait_for(&src, "live again", |f| f.is_live());
        std::fs::remove_file(&path).unwrap();
        let f4 = wait_for(&src, "down", |f| matches!(f.status, LinkStatus::Down(_)));
        assert_eq!(f4.status, LinkStatus::Down("no such file".into()));
        assert!(f4.snapshot.is_some());
        drop(src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_bad_json_and_schema() {
        let dir = tmpdir("bad");
        write_atomic(&dir, "a.json", "{not json");
        let src = Source::spawn(SourceSpec::File(dir.join("a.json")));
        let f = wait_for(&src, "bad json", |f| matches!(f.status, LinkStatus::Down(_)));
        assert_eq!(f.status, LinkStatus::Down("bad JSON".into()));
        write_atomic(&dir, "a.json", r#"{"schema":"llm-visuals.stats/2","seq":1}"#);
        let f = wait_for(&src, "schema", |f| {
            f.status == LinkStatus::Down("unsupported schema llm-visuals.stats/2".into())
        });
        assert!(f.snapshot.is_none());
        drop(src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unix_down_and_reconnect() {
        let dir = tmpdir("unix");
        let sock = dir.join("s.sock");
        let src = Source::spawn(SourceSpec::Unix(sock.clone()));
        wait_for(&src, "down", |f| matches!(f.status, LinkStatus::Down(_)));

        let l = UnixListener::bind(&sock).unwrap();
        let (mut c, _) = l.accept().unwrap();
        c.write_all(b"garbage\n").unwrap();
        c.write_all(format!("{}\n{}\n", line(1), line(2)).as_bytes()).unwrap();
        let f = wait_for(&src, "live", |f| f.is_live() && f.generation == 2);
        assert_eq!(f.snapshot.as_ref().unwrap().seq, Some(2));
        drop(c);
        drop(l);
        std::fs::remove_file(&sock).unwrap();
        wait_for(&src, "down after drop", |f| matches!(f.status, LinkStatus::Down(_)));

        let l = UnixListener::bind(&sock).unwrap();
        let (mut c, _) = l.accept().unwrap();
        c.write_all(format!("{}\n", line(3)).as_bytes()).unwrap();
        let f = wait_for(&src, "live again", |f| f.is_live() && f.generation == 3);
        assert_eq!(f.snapshot.as_ref().unwrap().seq, Some(3));
        drop(src);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Read request head; returns (request line, headers lowercased).
    fn read_head(c: &TcpStream) -> (String, Vec<String>) {
        let mut r = BufReader::new(c);
        let mut first = String::new();
        r.read_line(&mut first).unwrap();
        let mut hs = vec![];
        loop {
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            let l = l.trim_end().to_string();
            if l.is_empty() {
                break;
            }
            hs.push(l);
        }
        (first.trim_end().to_string(), hs)
    }

    fn chunk(s: &str) -> Vec<u8> {
        format!("{:x}\r\n{s}\r\n", s.len()).into_bytes()
    }

    #[test]
    fn http_auth_and_chunked_stream() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<String>)>();
        std::thread::spawn(move || {
            for stream in l.incoming() {
                let mut c = stream.unwrap();
                let (first, hs) = read_head(&c);
                let ok = hs.iter().any(|h| h == "Authorization: Bearer good");
                tx.send((first, hs)).unwrap();
                if !ok {
                    c.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                    continue;
                }
                c.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
                let a = line(1) + "\n";
                let b = line(2) + "\n";
                c.write_all(&chunk(&a)).unwrap();
                // chunk boundary in the middle of a line
                let (b1, b2) = b.split_at(20);
                c.write_all(&chunk(b1)).unwrap();
                c.flush().unwrap();
                std::thread::sleep(Duration::from_millis(100));
                c.write_all(&chunk(b2)).unwrap();
                std::thread::sleep(Duration::from_millis(300));
                // keep the connection open a bit, then close the stream cleanly
                c.write_all(b"0\r\n\r\n").unwrap();
            }
        });

        let url = format!("http://127.0.0.1:{port}");
        let bad = Source::spawn(SourceSpec::parse(&url, Some("wrong".into())).unwrap());
        let f = wait_for(&bad, "401", |f| matches!(f.status, LinkStatus::Down(_)));
        assert_eq!(f.status, LinkStatus::Down("401 unauthorized".into()));
        let (first, hs) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first, "GET /stream HTTP/1.1");
        assert!(hs.contains(&format!("Host: 127.0.0.1:{port}")), "{hs:?}");
        assert!(hs.contains(&"Authorization: Bearer wrong".to_string()));
        drop(bad);

        let good = Source::spawn(SourceSpec::parse(&url, Some("good".into())).unwrap());
        let f = wait_for(&good, "2 snapshots", |f| f.generation >= 2);
        assert_eq!(f.snapshot.as_ref().unwrap().seq, Some(2));
        assert!(f.is_live());
        // drain the header message(s) of the second client
        let mut seen = vec![];
        while let Ok(m) = rx.recv_timeout(Duration::from_millis(200)) {
            seen.push(m);
        }
        let (_, hs) = seen.iter().find(|(_, h)| h.contains(&"Authorization: Bearer good".to_string())).unwrap();
        assert!(hs.contains(&format!("Host: 127.0.0.1:{port}")));
        assert!(hs.iter().any(|h| h == "Connection: close"));
        // stream closed cleanly by the server -> Down, then reconnects (new snapshots)
        wait_for(&good, "reconnect", |f| f.generation >= 4);
        drop(good);
    }

    #[test]
    fn http_plain_body_and_chunk_decoder() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let _ = read_head(&c);
            let body = format!("{}\n", line(9));
            c.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n").unwrap();
            c.write_all(body.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        let src = Source::spawn(SourceSpec::parse(&format!("http://127.0.0.1:{port}/stream"), None).unwrap());
        let f = wait_for(&src, "plain", |f| f.generation >= 1);
        assert_eq!(f.snapshot.unwrap().seq, Some(9));

        let data = b"4;ext=1\r\nabcd\r\n3\r\nxyz\r\n0\r\n\r\n";
        let mut r = Chunked::new(BufReader::new(&data[..]));
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "abcdxyz");
    }
}
