//! What the PostgreSQL and key-value ports share about their sockets: what
//! to do when an accept fails, keepalive probes, the files the process may
//! open, and the memory their connections may hold together.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A pause after an accept that failed for want of resources.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// After a failed accept. Out of file descriptors (or memory), the
/// connection stays in the backlog and the next accept fails at once: a
/// pause, not a loop that burns a core while nobody is served. A connection
/// that the client dropped before it was accepted needs none.
pub(super) async fn accept_failed(e: &std::io::Error) {
    use std::io::ErrorKind::*;
    if !matches!(e.kind(), ConnectionAborted | ConnectionReset | Interrupted) {
        tokio::time::sleep(ACCEPT_PAUSE).await;
    }
}

/// Probes a connection that has gone quiet: a client whose machine vanished
/// without closing (a host gone, a router that forgot the flow) is found out
/// in about a minute and a half, instead of holding its place for days.
#[cfg(unix)]
pub(super) fn keep_alive(socket: &impl std::os::fd::AsRawFd) {
    let fd = socket.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| {
        // SAFETY: `value` lives through the call, which reads `size_of::<c_int>()` bytes of it.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&raw const value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    // The first probe after a minute of silence, then every 10 seconds, 3 of them.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 60);
    #[cfg(target_vendor = "apple")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, 60);
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 10);
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    }
}

/// On Windows, through Winsock (which the standard library links already):
/// the first probe after a minute of silence, then every 10 seconds (the
/// system sends 10 of them).
#[cfg(windows)]
pub(super) fn keep_alive(socket: &impl std::os::windows::io::AsRawSocket) {
    use std::ffi::c_void;
    /// Winsock's `tcp_keepalive`, times in milliseconds.
    #[repr(C)]
    struct TcpKeepalive {
        onoff: u32,
        keepalivetime: u32,
        keepaliveinterval: u32,
    }
    /// `_WSAIOW(IOC_VENDOR, 4)`.
    const SIO_KEEPALIVE_VALS: u32 = 0x9800_0004;
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAIoctl(
            socket: usize,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
            completion: *mut c_void,
        ) -> i32;
    }
    let values = TcpKeepalive { onoff: 1, keepalivetime: 60_000, keepaliveinterval: 10_000 };
    let mut returned = 0u32;
    // SAFETY: the socket is open for the call; `values` and `returned` live
    // through it, the input is read for its size, no output is written, and
    // the call is synchronous (no overlapped structure, no completion routine).
    unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_KEEPALIVE_VALS,
            (&raw const values).cast(),
            std::mem::size_of::<TcpKeepalive>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

/// Elsewhere the system's own settings apply: a vanished client is found
/// out at the next answer it is sent.
#[cfg(not(any(unix, windows)))]
pub(super) fn keep_alive<S>(_socket: &S) {}

/// How many files (sockets included) the system lets this process open,
/// when it sets a limit.
pub(super) fn open_files_limit() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `getrlimit` fills the struct it is given.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 && limit.rlim_cur != libc::RLIM_INFINITY {
            return Some(usize::try_from(limit.rlim_cur).unwrap_or(usize::MAX));
        }
    }
    None
}

/// Lets the process open as many files as the system allows it, once: the
/// default is often far less (256 on macOS, 1,024 on many Linux systems)
/// than the hard limit, and connections and the databases' files share it.
pub(super) fn raise_open_files_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: `getrlimit` fills the struct it is given.
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
                return;
            }
            // Plenty for this server; macOS refuses more than OPEN_MAX (10,240).
            let most: libc::rlim_t = if cfg!(target_vendor = "apple") { 10_240 } else { 65_536 };
            let wanted = limit.rlim_max.min(most);
            if wanted > limit.rlim_cur {
                limit.rlim_cur = wanted;
                // SAFETY: `setrlimit` reads the struct it is given. A refusal leaves the limit as it was.
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
            }
        });
    }
}

/// Key-value connections served at once by default.
pub(super) const KV_CONNECTIONS: usize = 256;

/// The connections a port serves at once: `wanted`, and no more than an
/// eighth of the files the process may open (the HTTP listener, the other
/// port and the databases' files need theirs).
pub(super) fn connections_within_files(wanted: usize) -> usize {
    let wanted = wanted.max(1);
    open_files_limit().map_or(wanted, |files| wanted.min((files / 8).max(16)))
}

/// Adds `n` to `count` when it stays within `most`; `false` (and nothing
/// added) otherwise. Exact however many threads add at once.
pub(super) fn add_within(count: &AtomicUsize, n: usize, most: usize) -> bool {
    let mut now = count.load(Ordering::Relaxed);
    loop {
        let Some(next) = now.checked_add(n).filter(|next| *next <= most) else { return false };
        match count.compare_exchange_weak(now, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => now = actual,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// A share of a port's memory is taken and given back in steps of this: one
/// shared counter per step, not per byte.
const STEP: usize = 64 << 10;
/// How long a connection waits for memory other connections hold before it
/// is refused: long enough for a burst to pass.
const MEMORY_WAIT: Duration = Duration::from_secs(2);

/// What the connections of one port may hold together beyond the little
/// each needs (a message being read past its first 64 KiB, what a session
/// keeps between messages): past it a client is refused, instead of the
/// server running out of memory.
pub(super) struct PortMemory {
    used: AtomicUsize,
    total: usize,
}

impl PortMemory {
    /// `setting` bytes; by default an eighth of the server's memory (128 MiB
    /// when it is unknown), and room for four of the largest messages: the
    /// HTTP requests, the statements at work and the page caches have the rest.
    pub(super) fn new(setting: Option<usize>, max_body: usize) -> Arc<PortMemory> {
        let total = match setting {
            Some(bytes) => bytes.max(1 << 20),
            None => crate::memory::MemoryInfo::now()
                .total_bytes
                .map_or(128 << 20, |m| usize::try_from(m / 8).unwrap_or(usize::MAX))
                .max(max_body.saturating_mul(4)),
        };
        Arc::new(PortMemory { used: AtomicUsize::new(0), total })
    }

    /// What the connections may hold together, in bytes.
    pub(super) fn total(&self) -> usize {
        self.total
    }

    fn take(&self, bytes: usize) -> bool {
        add_within(&self.used, bytes, self.total)
    }

    fn give(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// A connection's part of its port's memory: given back when it is dropped.
pub(super) struct Share {
    memory: Arc<PortMemory>,
    held: usize,
}

impl Share {
    pub(super) fn new(memory: &Arc<PortMemory>) -> Share {
        Share { memory: memory.clone(), held: 0 }
    }

    /// The port's memory this share may never exceed, in bytes.
    pub(super) fn total(&self) -> usize {
        self.memory.total
    }

    /// What it holds now, in bytes.
    pub(super) fn held(&self) -> usize {
        self.held
    }

    /// Holds `bytes` in all, more or less than now: `false`, and what was
    /// held kept, when the port has not that much left.
    pub(super) fn hold(&mut self, bytes: usize) -> bool {
        // The common case, a session that holds nothing of the port's: no
        // shared counter to touch.
        if bytes == self.held {
            return true;
        }
        if bytes > self.held {
            let wanted = bytes.next_multiple_of(STEP).min(bytes.max(self.memory.total));
            if !self.memory.take(wanted - self.held) {
                return false;
            }
            self.held = wanted;
        } else if bytes == 0 || bytes + STEP <= self.held {
            let kept = bytes.next_multiple_of(STEP).min(self.held);
            self.memory.give(self.held - kept);
            self.held = kept;
        }
        true
    }

    /// [`Share::hold`], waiting a moment for other connections to give some
    /// back (a thread of the PostgreSQL port).
    pub(super) fn hold_waiting(&mut self, bytes: usize) -> bool {
        let until = Instant::now() + MEMORY_WAIT;
        while !self.hold(bytes) {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// [`Share::hold`], waiting a moment for other connections to give some
    /// back (a task of the key-value port).
    pub(super) async fn hold_soon(&mut self, bytes: usize) -> bool {
        let until = tokio::time::Instant::now() + MEMORY_WAIT;
        while !self.hold(bytes) {
            if tokio::time::Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.memory.give(self.held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_within_the_files_the_process_may_open() {
        let n = connections_within_files(KV_CONNECTIONS);
        assert!((1..=KV_CONNECTIONS).contains(&n), "{n}");
        if let Some(files) = open_files_limit() {
            assert!(n <= (files / 8).max(16), "{n} for {files} files");
        }
        assert_eq!(connections_within_files(0), 1);
    }

    /// Shares take what the port has, no more, and give it back.
    #[test]
    fn shares_of_a_port() {
        let memory = PortMemory::new(Some(1 << 20), 0);
        let (mut a, mut b) = (Share::new(&memory), Share::new(&memory));
        assert!(a.hold(700 << 10));
        assert!(!b.hold(400 << 10), "past what the port has");
        assert!(b.hold(300 << 10));
        assert!(!a.hold(800 << 10));
        assert!(a.hold(10));
        assert!(b.hold(900 << 10), "a gave back what it no longer holds");
        drop(b);
        assert!(a.hold(1 << 20));
        assert!(!Share::new(&memory).hold(1));
        assert!(a.hold(0));
        assert_eq!(memory.used.load(Ordering::Relaxed), 0);
        let started = Instant::now();
        let mut c = Share::new(&memory);
        let mut d = Share::new(&memory);
        assert!(c.hold(1 << 20));
        assert!(!d.hold_waiting(1), "held for good");
        assert!(started.elapsed() >= MEMORY_WAIT);
    }

    #[cfg(unix)]
    #[test]
    fn keep_alive_is_set() {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let get = |level: libc::c_int, name: libc::c_int| {
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: `value` and `len` live through the call, which writes at most `len` bytes.
            let r = unsafe { libc::getsockopt(client.as_raw_fd(), level, name, (&raw mut value).cast(), &mut len) };
            assert_eq!(r, 0);
            value
        };
        assert_eq!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 0);
        keep_alive(&client);
        assert_ne!(get(libc::SOL_SOCKET, libc::SO_KEEPALIVE), 0);
        #[cfg(target_os = "linux")]
        assert_eq!((get(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), get(libc::IPPROTO_TCP, libc::TCP_KEEPCNT)), (60, 3));
    }
}
