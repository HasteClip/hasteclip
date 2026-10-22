//! How far an upload has really come, and whether it has stalled, from what
//! the network itself says.
//!
//! What the app writes into a socket goes into the system's send buffer
//! first (megabytes of it), so counting writes runs ahead of what's gone out
//! and stands still for seconds on a slow line while all is well. The other
//! end's acknowledgements don't: everything written that has left the send
//! buffer has been confirmed, and the system knows how long a round trip
//! takes on the connection. Progress is counted from what's confirmed, and a
//! connection has stalled when bytes are waiting and none are confirmed for
//! several of TCP's own resend waits: what this connection, at its speed,
//! shows to be wrong. No fixed limit.
//!
//! Each upload lane (one part at a time) has its own agent, whose TCP
//! connections go through [`Keeping`]; [`Lane::watch`] watches them while a
//! request runs, and shuts them when they've stalled, so the request ends.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport};

/// A connection the lane opened, and how many bytes were written into it.
struct Kept {
    stream: TcpStream,
    written: Arc<AtomicU64>,
}

/// The lane's connections, for [`Watch`] to ask about.
#[derive(Clone, Default)]
pub struct Sockets(Arc<Mutex<Vec<Kept>>>);

impl std::fmt::Debug for Sockets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sockets")
    }
}

/// What the system says about one connection.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stats {
    /// Bytes the other end has confirmed, since the connection opened.
    acked: u64,
    /// Bytes written and not confirmed yet.
    waiting: u64,
    /// TCP's wait before it sends unconfirmed bytes again, from this
    /// connection's round trips (RFC 6298).
    rto: Duration,
}

impl Sockets {
    fn stats(&self) -> Vec<(std::net::SocketAddr, Stats)> {
        let all = self.0.lock().unwrap();
        all.iter().filter_map(|k| Some((k.stream.local_addr().ok()?, imp::stats(&k.stream, k.written.load(Ordering::Relaxed))?))).collect()
    }

    /// End every connection: a request blocked on one returns at once.
    fn shut(&self) {
        for k in self.0.lock().unwrap().drain(..) {
            let _ = k.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Keepalives on `stream`: after a second with nothing heard, a probe every
/// second, the connection ended after 5 unanswered. Seconds are the finest
/// the systems take.
fn keepalive(stream: &TcpStream) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = stream.as_raw_fd();
        #[cfg(target_os = "macos")]
        let idle = libc::TCP_KEEPALIVE;
        #[cfg(not(target_os = "macos"))]
        let idle = libc::TCP_KEEPIDLE;
        for (level, name, value) in [(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1), (libc::IPPROTO_TCP, idle, 1), (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 1), (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 5)] {
            let v: libc::c_int = value;
            // SAFETY: an int option on a socket we own.
            unsafe { libc::setsockopt(fd, level, name, (&raw const v).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t) };
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{SIO_KEEPALIVE_VALS, SOCKET, WSAIoctl, tcp_keepalive};
        // Windows probes 10 times, at these intervals (in ms).
        let vals = tcp_keepalive { onoff: 1, keepalivetime: 1000, keepaliveinterval: 1000 };
        let mut returned = 0u32;
        // SAFETY: a tcp_keepalive in, nothing out, on a socket we own.
        unsafe {
            WSAIoctl(SOCKET(stream.as_raw_socket() as usize), SIO_KEEPALIVE_VALS, Some((&raw const vals).cast()), std::mem::size_of::<tcp_keepalive>() as u32, None, 0, &mut returned, None, None);
        }
    }
}

/// TCP's own rule for its resend wait (RFC 6298), with Linux's and macOS's
/// 200 ms floor.
fn rto(srtt: Duration, rttvar: Duration) -> Duration {
    (srtt + (rttvar * 4).max(Duration::from_millis(1))).max(Duration::from_millis(200))
}

/// The lane's TCP connector: connects as ureq's own does, and keeps each socket.
#[derive(Debug, Clone)]
pub struct Keeping(Sockets);

impl<In: Transport> Connector<In> for Keeping {
    type Out = Either<In, Tcp>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        if let Some(c) = chained {
            return Ok(Some(Either::A(c)));
        }
        let config = details.config;
        let mut last = None;
        for addr in details.addrs.iter() {
            let stream = match details.timeout.not_zero() {
                Some(t) => TcpStream::connect_timeout(addr, *t),
                None => TcpStream::connect(addr),
            };
            match stream {
                Ok(s) => {
                    if config.no_delay() {
                        s.set_nodelay(true)?;
                    }
                    // Waiting for an answer, the other end is asked whether
                    // it's still there every second, as often as the system
                    // allows: one that's gone ends the connection in seconds,
                    // one that's thinking can take as long as it likes.
                    keepalive(&s);
                    let written = Arc::new(AtomicU64::new(0));
                    if let Ok(kept) = s.try_clone() {
                        self.0.0.lock().unwrap().push(Kept { stream: kept, written: written.clone() });
                    }
                    let buffers = LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size());
                    return Ok(Some(Either::B(Tcp { stream: s, buffers, written })));
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or(ureq::Error::ConnectionFailed, ureq::Error::from))
    }
}

/// A TCP connection, as ureq's own transport is, counting what's written.
pub struct Tcp {
    stream: TcpStream,
    buffers: LazyBuffers,
    written: Arc<AtomicU64>,
}

impl std::fmt::Debug for Tcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tcp").field("addr", &self.stream.peer_addr().ok()).finish()
    }
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}

impl Transport for Tcp {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.set_write_timeout(timeout.not_zero().map(|t| *t))?;
        let mut output = &self.buffers.output()[..amount];
        // Counted as each write goes into the send buffer, so what's
        // confirmed is never more than what's counted.
        while !output.is_empty() {
            match self.stream.write(output) {
                Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
                Ok(n) => {
                    self.written.fetch_add(n as u64, Ordering::Relaxed);
                    output = &output[n..];
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if timed_out(&e) => return Err(ureq::Error::Timeout(timeout.reason)),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.set_read_timeout(timeout.not_zero().map(|t| *t))?;
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input).map_err(|e| if timed_out(&e) { ureq::Error::Timeout(timeout.reason) } else { ureq::Error::from(e) })?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        // Nothing should be waiting to be read between requests: anything
        // there, or the other end gone (or shut by a stall), and it can't be
        // used again.
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut buf = [0];
        let open = matches!(self.stream.read(&mut buf), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock);
        open && self.stream.set_nonblocking(false).is_ok()
    }
}

/// One lane of an upload: its own agent and connections, so what its
/// connections confirm is its request's.
pub struct Lane {
    pub agent: ureq::Agent,
    sockets: Sockets,
}

impl Lane {
    /// ureq's usual chain (proxies, TLS) with the lane's TCP connections,
    /// and no time limit: [`Lane::watch`] says when it's stalled.
    pub fn new() -> Self {
        use ureq::unversioned::resolver::DefaultResolver;
        use ureq::unversioned::transport::{ConnectProxyConnector, RustlsConnector};
        let sockets = Sockets::default();
        let config = ureq::Agent::config_builder().http_status_as_error(false).max_idle_connections_per_host(1).build();
        let connector = ().chain(ConnectProxyConnector::default()).chain(Keeping(sockets.clone())).chain(RustlsConnector::default());
        Self { agent: ureq::Agent::with_parts(config, connector, DefaultResolver::default()), sockets }
    }

    /// Run `request` on this lane, calling `confirmed` with how many of its
    /// bytes the other end has confirmed as that grows (headers and TLS
    /// included). Its connections are shut when they've stalled: bytes
    /// waiting and none confirmed for [`STALL_WAITS`] of TCP's resend waits
    /// on this connection, times `patience`. A receiver that's slow and one
    /// that's gone look the same to TCP until then; one cut off that was
    /// only slow is tried again with twice the patience, as TCP backs off.
    /// Once everything is sent, the answer is waited for as long as it
    /// takes: a keepalive (see [`Keeping`]) ends a connection to one that's
    /// gone. Returns the request's result and whether a stall ended it.
    pub fn watch<T>(&self, patience: u32, mut confirmed: impl FnMut(u64) + Send, request: impl FnOnce(&ureq::Agent) -> T) -> (T, bool) {
        let done = AtomicBool::new(false);
        let stalled = AtomicBool::new(false);
        let before: Vec<_> = self.sockets.stats().into_iter().map(|(a, s)| (a, s.acked)).collect();
        let result = std::thread::scope(|s| {
            let watcher = s.spawn(|| {
                let mut most = 0u64;
                let mut moved = Instant::now();
                let mut out = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    let now = self.sockets.stats();
                    let total: u64 = now.iter().map(|(a, s)| s.acked - before.iter().find(|(b, _)| b == a).map_or(0, |(_, at)| *at).min(s.acked)).sum();
                    let waiting = now.iter().any(|(_, s)| s.waiting > 0);
                    let rto = now.iter().map(|(_, s)| s.rto).max().unwrap_or(Duration::from_secs(1));
                    if total > most {
                        most = total;
                        moved = Instant::now();
                        out.push(total);
                    }
                    if !waiting {
                        // Everything sent: the answer is up to the other end.
                        moved = Instant::now();
                    } else if moved.elapsed() > rto * STALL_WAITS * patience.max(1) {
                        stalled.store(true, Ordering::Relaxed);
                        self.sockets.shut();
                        break;
                    }
                    for n in out.drain(..) {
                        confirmed(n);
                    }
                    // A few times a resend wait, at most every 50 ms: cheap.
                    std::thread::sleep((rto / 8).clamp(Duration::from_millis(5), Duration::from_millis(50)));
                }
            });
            let r = request(&self.agent);
            done.store(true, Ordering::Relaxed);
            let _ = watcher.join();
            r
        });
        (result, stalled.load(Ordering::Relaxed))
    }
}

/// How many of TCP's resend waits with nothing confirmed make a stall. TCP
/// doubles its wait each time it sends again, so four in a row is a line
/// gone for 15 of them: not a hiccup.
pub const STALL_WAITS: u32 = 4;

#[cfg(target_os = "macos")]
mod imp {
    use super::Stats;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    /// macOS's `struct tcp_connection_info`: 112 bytes, its fast-open flags
    /// a bitfield (libc's struct spells them out as words, so its counters
    /// land past what macOS fills in).
    const SNDBUF_BYTES: usize = 32;
    const SRTT: usize = 44;
    const RTTVAR: usize = 48;

    pub(super) fn stats(stream: &std::net::TcpStream, written: u64) -> Option<Stats> {
        let mut buf = [0u8; 112];
        let mut len = buf.len() as libc::socklen_t;
        // SAFETY: a buffer the size macOS fills, for a socket we own.
        let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_CONNECTION_INFO, buf.as_mut_ptr().cast(), &mut len) };
        if r != 0 || (len as usize) < RTTVAR + 4 {
            return None;
        }
        let u32_at = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        // The send buffer holds what's written and not confirmed yet.
        let waiting = u64::from(u32_at(SNDBUF_BYTES));
        let ms = |v: u32| Duration::from_millis(u64::from(v));
        Some(Stats { acked: written.saturating_sub(waiting), waiting, rto: super::rto(ms(u32_at(SRTT)), ms(u32_at(RTTVAR))) })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Stats;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    pub(super) fn stats(stream: &std::net::TcpStream, written: u64) -> Option<Stats> {
        // SAFETY: a zeroed C struct, filled by the system for a socket we own.
        unsafe {
            let mut info: libc::tcp_info = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
            let r = libc::getsockopt(stream.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_INFO, (&raw mut info).cast(), &mut len);
            if r != 0 {
                return None;
            }
            let acked = info.tcpi_bytes_acked.min(written);
            Some(Stats { acked, waiting: written - acked, rto: Duration::from_micros(u64::from(info.tcpi_rto)) })
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::Stats;
    use std::os::windows::io::AsRawSocket;
    use std::time::Duration;
    use windows::Win32::Networking::WinSock::{SIO_TCP_INFO, SOCKET, TCP_INFO_v0, WSAIoctl};

    pub(super) fn stats(stream: &std::net::TcpStream, written: u64) -> Option<Stats> {
        let mut version: u32 = 0;
        let mut info = TCP_INFO_v0::default();
        let mut returned: u32 = 0;
        // SAFETY: in and out buffers sized for SIO_TCP_INFO version 0, on a
        // socket we own; no overlapped I/O.
        let r = unsafe {
            WSAIoctl(
                SOCKET(stream.as_raw_socket() as usize),
                SIO_TCP_INFO,
                Some((&raw mut version).cast()),
                std::mem::size_of::<u32>() as u32,
                Some((&raw mut info).cast()),
                std::mem::size_of::<TCP_INFO_v0>() as u32,
                &mut returned,
                None,
                None,
            )
        };
        if r != 0 {
            return None;
        }
        // Sent, less what was sent again and what's still on its way.
        let sent = info.BytesOut.saturating_sub(u64::from(info.BytesRetrans)).saturating_sub(u64::from(info.BytesInFlight));
        let acked = sent.min(written);
        // No variation to read: RFC 6298's start, half the round trip.
        let rtt = Duration::from_micros(u64::from(info.RttUs));
        Some(Stats { acked, waiting: written - acked, rto: super::rto(rtt, rtt / 2) })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod imp {
    pub(super) fn stats(_: &std::net::TcpStream, _: u64) -> Option<super::Stats> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A listener whose connections take what a server's would over the
    /// internet: a local connection's buffer otherwise grows to megabytes at
    /// memory speed and takes a whole part at once. Set before it's bound,
    /// or macOS doesn't keep it.
    #[cfg(unix)]
    pub(crate) fn small_buffer_listener() -> TcpListener {
        use std::os::fd::FromRawFd;
        // SAFETY: a new socket we own, handed to TcpListener once listening.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            let size: libc::c_int = 64 * 1024;
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVBUF, (&raw const size).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as _;
            addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            #[cfg(target_os = "macos")]
            {
                addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
            }
            assert_eq!(libc::bind(fd, (&raw const addr).cast(), std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t), 0);
            assert_eq!(libc::listen(fd, 1), 0);
            TcpListener::from_raw_fd(fd)
        }
    }
    #[cfg(not(unix))]
    pub(crate) fn small_buffer_listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").unwrap()
    }

    /// A plain HTTP server for one PUT: reads the headers, then the body at
    /// `rate` bytes a second, stopping for good at `stop_at` bytes (it stays
    /// connected, reading nothing), and answers 200 once it has it all.
    fn server(rate: f64, stop_at: Option<usize>) -> (String, std::thread::JoinHandle<()>) {
        let listener = small_buffer_listener();
        let url = format!("http://{}/part", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && c.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head).to_lowercase();
            let len: usize = text.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let t0 = Instant::now();
            let (mut got, mut buf) = (0usize, [0u8; 4096]);
            while got < len {
                if stop_at.is_some_and(|s| got >= s) {
                    std::thread::sleep(Duration::from_secs(30));
                    return;
                }
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got += n,
                }
                let ahead = got as f64 / rate - t0.elapsed().as_secs_f64();
                if ahead > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(ahead));
                }
            }
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 0\r\n\r\n");
        });
        (url, handle)
    }

    /// 1 MB at 300 kB/s, read in bursts (so what's confirmed pauses while
    /// all is well): it goes through, with what's confirmed growing as it
    /// goes, and isn't taken for a stall.
    #[test]
    fn a_slow_upload_is_no_stall() {
        let (url, server) = server(300_000.0, None);
        let lane = Lane::new();
        let body = vec![7u8; 1_000_000];
        let seen = Mutex::new(Vec::new());
        let t0 = Instant::now();
        let (res, stalled) = lane.watch(1, |n| seen.lock().unwrap().push((t0.elapsed(), n)), |agent| agent.put(&url).send(&body[..]));
        let _ = server.join();
        assert!(!stalled, "a slow upload taken for a stall");
        assert_eq!(res.unwrap().status(), 200);
        let seen = seen.into_inner().unwrap();
        // It moves all the way through, a little at a time, as the server
        // takes it (its receive buffer at a time, here).
        let gaps = seen.windows(2).map(|w| w[1].0 - w[0].0);
        assert!(gaps.clone().all(|g| g < Duration::from_millis(600)), "a gap in progress: {:?}", gaps.max());
        assert!(seen.windows(2).all(|w| w[1].1 - w[0].1 < 200_000), "progress jumped");
        let half = seen.iter().find(|(_, n)| *n >= 500_000).map(|(t, _)| *t).expect("half confirmed");
        assert!(half > Duration::from_millis(1000), "half confirmed at {half:?}: follows the server, not the writes");
    }

    /// The server stops reading a third of the way in and stays connected:
    /// the request ends as a stall, a few of this line's normal pauses after.
    #[test]
    fn a_stopped_upload_is_a_stall() {
        let (url, _server) = server(300_000.0, Some(300_000));
        let lane = Lane::new();
        let body = vec![7u8; 1_000_000];
        let t0 = Instant::now();
        let (res, stalled) = lane.watch(1, |_| {}, |agent| agent.put(&url).send(&body[..]));
        let took = t0.elapsed();
        assert!(stalled, "the stop wasn't seen");
        assert!(res.is_err());
        // About 1 s to stop, then a few of its normal pauses.
        assert!(took < Duration::from_secs(10), "seen after {took:?}");
    }
}
