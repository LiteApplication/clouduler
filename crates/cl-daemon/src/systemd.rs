//! Talking to the service manager: readiness, status, reload and watchdog.
//!
//! Everything here is a no-op when the daemon is not run by systemd (no
//! `NOTIFY_SOCKET`), so a daemon started by hand, in a test, or from a unit
//! still written as `Type=simple` behaves exactly as before.

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use tracing::debug;

/// Send one `sd_notify` datagram. Best effort: a service manager that went
/// away is not a reason to stop serving.
pub fn notify(message: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send(&path, message) {
        debug!(error = %e, "cannot notify the service manager");
    }
}

fn send(path: &std::ffi::OsStr, message: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::SocketAddr;

    let bytes = path.as_bytes();
    let addr = match bytes.strip_prefix(b"@") {
        // A leading `@` is the abstract namespace, written that way because
        // the real name starts with a NUL byte.
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(path)?,
    };
    let socket = UnixDatagram::unbound()?;
    socket.send_to_addr(message.as_bytes(), &addr)?;
    Ok(())
}

/// TCP listeners systemd opened for us (socket activation), in order.
///
/// The `sd_listen_fds` protocol: `LISTEN_PID` names the process the sockets
/// were meant for and `LISTEN_FDS` how many there are, starting at fd 3.
/// Because the socket unit outlives the service, these stay bound, and keep
/// queueing connections, while the daemon restarts. Empty when not socket
/// activated. Taken once: the variables are cleared so children do not see
/// them, and the descriptors are marked close-on-exec.
pub fn take_listen_fds() -> Vec<std::net::TcpListener> {
    use std::os::fd::{FromRawFd, IntoRawFd, RawFd};

    const FIRST_FD: RawFd = 3;
    let mine = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
        == Some(std::process::id());
    let count = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse::<RawFd>().ok())
        .unwrap_or(0);
    for var in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        std::env::remove_var(var);
    }
    if !mine || count <= 0 {
        return Vec::new();
    }

    let mut listeners = Vec::new();
    for fd in FIRST_FD..FIRST_FD + count {
        let _ = nix::fcntl::fcntl(
            // Borrowed only for this call; ownership is taken just below.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );
        // SAFETY: systemd handed us this descriptor and, per the protocol,
        // nothing else in the process owns it.
        let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
        if listener.local_addr().is_ok() && listener.set_nonblocking(true).is_ok() {
            listeners.push(listener);
        } else {
            // Not a TCP socket we can use: leave it open for whoever it is for.
            let _ = listener.into_raw_fd();
        }
    }
    listeners
}

/// The daemon is up and serving. `STATUS` is what `systemctl status` shows.
pub fn ready(status: &str) {
    notify(&format!("READY=1\nSTATUS={status}"));
}

pub fn status(status: &str) {
    notify(&format!("STATUS={status}"));
}

pub fn stopping() {
    notify("STOPPING=1\nSTATUS=shutting down");
}

/// Bracket a configuration reload so `systemctl reload` waits for it and
/// reports the unit as reloading rather than active meanwhile.
pub fn reloading() {
    let usec = monotonic_usec();
    notify(&format!(
        "RELOADING=1\nMONOTONIC_USEC={usec}\nSTATUS=reloading"
    ));
}

fn monotonic_usec() -> u128 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map(|t| t.tv_sec() as u128 * 1_000_000 + t.tv_nsec() as u128 / 1_000)
        .unwrap_or_default()
}

/// Ask for more time to start, for the slow steps (waiting on dockerd).
pub fn extend_timeout(by: Duration) {
    notify(&format!("EXTEND_TIMEOUT_USEC={}", by.as_micros()));
}

/// How often to ping, if the unit set `WatchdogSec=`: half the deadline, as
/// `sd_watchdog_enabled(3)` recommends.
pub fn watchdog_interval() -> Option<Duration> {
    interval_from(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )
}

fn interval_from(usec: Option<&str>, pid: Option<&str>, own_pid: u32) -> Option<Duration> {
    // Set for a specific process; inherited into a child that is not it.
    if let Some(pid) = pid {
        if pid.parse::<u32>().ok()? != own_pid {
            return None;
        }
    }
    let usec: u64 = usec?.parse().ok()?;
    (usec > 0).then(|| Duration::from_micros(usec / 2))
}

/// Pings the watchdog from the runtime itself. Because it runs as an ordinary
/// task, a runtime starved by blocking work (which has wedged this daemon
/// before) stops pinging and systemd restarts it, which is the point.
pub fn spawn_watchdog() -> Option<tokio::task::JoinHandle<()>> {
    let every = watchdog_interval()?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        loop {
            ticker.tick().await;
            notify("WATCHDOG=1");
        }
    }))
}

/// What the service manager or an operator asked of a running daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Stop,
    Reload,
}

/// SIGTERM (`systemctl stop`, `restart`) and SIGINT stop the daemon cleanly;
/// SIGHUP (`systemctl reload`) reloads it.
pub struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    /// Install the handlers. Done at the very start of the daemon, so a stop
    /// arriving while it is still starting is queued instead of killing it
    /// with the drive mounts half set up.
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    pub async fn next(&mut self) -> Request {
        tokio::select! {
            _ = self.term.recv() => Request::Stop,
            _ = self.int.recv() => Request::Stop,
            _ = self.hup.recv() => Request::Reload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watchdog_pings_at_half_the_deadline() {
        assert_eq!(
            interval_from(Some("60000000"), None, 1),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn no_watchdog_when_unset_or_zero() {
        assert_eq!(interval_from(None, None, 1), None);
        assert_eq!(interval_from(Some("0"), None, 1), None);
        assert_eq!(interval_from(Some("garbage"), None, 1), None);
    }

    #[test]
    fn a_watchdog_meant_for_another_process_is_ignored() {
        assert_eq!(interval_from(Some("60000000"), Some("42"), 7), None);
        assert!(interval_from(Some("60000000"), Some("7"), 7).is_some());
    }

    #[test]
    fn notifications_reach_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).expect("bind");
        send(path.as_os_str(), "READY=1").expect("send");
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).expect("recv");
        assert_eq!(&buf[..n], b"READY=1");
    }
}
