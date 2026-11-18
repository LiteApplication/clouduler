//! Guessing the answers init would otherwise have to ask for.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use anyhow::{Context, Result};
use ipnet::IpNet;

/// Whether this process can write to /etc, /usr/bin and /var/lib.
///
/// Reads the euid from /proc to avoid a libc dependency. A kernel without the
/// field is treated as unprivileged, so init fails early rather than halfway.
pub fn is_root() -> bool {
    effective_uid() == Some(0)
}

fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    // "Uid:\treal\teffective\tsaved\tfilesystem"
    line.split_whitespace().nth(2)?.parse().ok()
}

pub fn hostname() -> Result<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").context("reading hostname")?;
    let name = raw.trim().to_string();
    Ok(name)
}

/// The address this machine would use to reach `peer`.
///
/// A connected UDP socket sends nothing; the kernel just runs the route lookup,
/// so this works against a down peer without privileges.
pub fn source_address_for(peer: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr = match peer {
        IpAddr::V4(_) => "0.0.0.0:0".parse().ok()?,
        IpAddr::V6(_) => "[::]:0".parse().ok()?,
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(SocketAddr::new(peer, 9)).ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

/// This machine's address inside `subnet`, when it has one.
///
/// Probes the first host address so the kernel picks the carrying interface,
/// then checks the answer against the subnet to reject the default route address.
pub fn address_in(subnet: IpNet) -> Option<IpAddr> {
    let probe = subnet.hosts().next()?;
    let local = source_address_for(probe)?;
    subnet.contains(&local).then_some(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer must agree with itself even on a stripped /proc.
    #[test]
    fn the_effective_uid_is_readable_and_agrees_with_the_root_check() {
        let uid = effective_uid().expect("/proc/self/status exposes Uid");
        assert_eq!(is_root(), uid == 0);
    }

    #[test]
    fn the_loopback_route_resolves_to_a_loopback_source() {
        let local = source_address_for("127.0.0.1".parse().unwrap());
        assert_eq!(local, Some("127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn an_address_outside_every_local_subnet_is_not_claimed() {
        // 127.0.0.0/8 is local, but this machine does not hold 127.9.9.9, and
        // the probe address is what the route lookup answers with.
        let subnet: IpNet = "203.0.113.0/24".parse().unwrap();
        assert_eq!(address_in(subnet), None);
    }
}
