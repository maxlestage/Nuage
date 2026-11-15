//! What the PostgreSQL and key-value ports share about their sockets.

use std::time::Duration;

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

/// Elsewhere the system's own settings apply: a vanished client is found
/// out at the next answer it is sent.
#[cfg(not(unix))]
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

/// Key-value connections served at once by default: 1,000, and a quarter
/// of the files the process may open when that is less (the HTTP listener,
/// the PostgreSQL port and the databases' files need theirs).
pub(super) fn default_kv_connections() -> usize {
    open_files_limit().map_or(1000, |files| (files / 4).clamp(16, 1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_within_the_files_the_process_may_open() {
        let n = default_kv_connections();
        assert!((16..=1000).contains(&n), "{n}");
        if let Some(files) = open_files_limit() {
            assert!(n <= (files / 4).max(16), "{n} for {files} files");
        }
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
