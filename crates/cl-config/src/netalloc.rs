//! Name-derived network allocations shared by cluster validation and the
//! daemon: the pool octet an egress/nat bridge subnet lands on, and the policy
//! routing table an egress network uses.
//!
//! These are hashes of the network name so every node computes the same answer
//! without coordinating. The price is that two names can collide, silently
//! sharing a subnet or a routing table. Validation (`ClusterConfig::validate`)
//! uses these same functions to refuse such a config up front, which is why
//! they live here and not in `cl-net`, which depends on this crate.

use std::hash::{Hash, Hasher};

/// Second octet of the pool reserved for egress (and wireguard) bridges.
pub const EGRESS_POOL_BASE: u8 = 245;

/// Second octet of the pool reserved for nat-kind bridges. Its own base so a
/// nat and an egress network that hash to the same octet still never share a
/// subnet.
pub const NAT_POOL_BASE: u8 = 246;

/// First policy routing table id handed to egress networks.
pub const EGRESS_TABLE_BASE: u32 = 5000;

/// Linux interface names hold `IFNAMSIZ - 1` usable bytes.
pub const IFNAME_MAX_BYTES: usize = 15;

/// FNV-1a, not cryptographic: it only has to spread names across a small space
/// deterministically.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// The third octet of a network's `/24` inside the egress or nat pool. The
/// same hash serves both pools; the pool base is what keeps them apart.
pub fn subnet_octet(network: &str) -> u8 {
    (fnv1a(network.as_bytes()) % 256) as u8
}

/// The hasher behind every derived identifier that gets persisted: policy
/// routing tables in the kernel, the revision label on containers and NFS
/// fsids held by clients. SipHash-1-3 with zero keys is what
/// `DefaultHasher::new()` happened to be when those values were first handed
/// out, but std documents its algorithm as unspecified, so this pins it. The
/// output must never change, or live networks, containers and mounts drift.
pub fn stable_hasher() -> siphasher::sip::SipHasher13 {
    siphasher::sip::SipHasher13::new_with_keys(0, 0)
}

/// The policy routing table an egress or wireguard network's bridge looks up.
///
/// Deliberately the SipHash the daemon always used for this, not FNV: the
/// ids of networks that already exist on live nodes must not change.
pub fn egress_table_id(network: &str) -> u32 {
    let mut hasher = stable_hasher();
    network.hash(&mut hasher);
    EGRESS_TABLE_BASE + (hasher.finish() % 1000) as u32
}

/// Checks a host interface name the way the kernel will: non-empty, at most 15
/// bytes, none of `/`, whitespace or `:` (which `ip` and `iptables` also
/// reject or misparse), and only `[A-Za-z0-9_.-]`. Interface names reach
/// `ip`, `iptables` and `/proc/sys/net/ipv4/conf/<name>/`, so a name like
/// `../../x` must never get that far.
pub fn check_interface_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("must not be empty".to_string());
    }
    if name.len() > IFNAME_MAX_BYTES {
        return Err(format!(
            "{name:?} is {} bytes, Linux interface names are at most {IFNAME_MAX_BYTES}",
            name.len()
        ));
    }
    if name == "." || name == ".." {
        return Err(format!("{name:?} is not a usable interface name"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    {
        return Err(format!(
            "{name:?} contains {bad:?}, only letters, digits, '_', '.' and '-' are allowed"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    // Guards the move off `DefaultHasher` on the current toolchain. If a future
    // rustc changes `DefaultHasher`, this may start failing: that is expected,
    // delete it then and keep the hardcoded values below.
    #[test]
    fn stable_hasher_matches_default_hasher_today() {
        for name in ["", "a", "vpn", "egress-net", "wg0", "réseau"] {
            let mut old = DefaultHasher::new();
            name.hash(&mut old);
            let mut new = stable_hasher();
            name.hash(&mut new);
            assert_eq!(old.finish(), new.finish(), "{name:?}");
        }
    }

    #[test]
    fn stable_hasher_values_are_pinned() {
        let hash = |s: &str| {
            let mut h = stable_hasher();
            s.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash("vpn"), 8819488551843405589);
        assert_eq!(hash("egress-net"), 7284548392039946455);
        assert_eq!(egress_table_id("vpn"), 5589);
        assert_eq!(egress_table_id("egress-net"), 5455);
    }

    #[test]
    fn interface_names_are_checked_like_the_kernel_does() {
        assert!(check_interface_name("wg0").is_ok());
        assert!(check_interface_name("wg-my_vpn.1").is_ok());
        assert!(check_interface_name("").is_err());
        assert!(check_interface_name("a/b").is_err());
        assert!(check_interface_name("../x").is_err());
        assert!(check_interface_name("has space").is_err());
        assert!(check_interface_name("a:b").is_err());
        assert!(check_interface_name("abcdefghijklmnop").is_err());
        assert!(check_interface_name("abcdefghijklmno").is_ok());
        // Five two-byte chars are ten chars but twenty bytes of name.
        assert!(check_interface_name("éééééééé").is_err());
    }

    #[test]
    fn the_table_id_stays_in_its_range_and_is_stable() {
        let id = egress_table_id("vpn");
        assert!((5000..6000).contains(&id));
        assert_eq!(id, egress_table_id("vpn"));
    }
}
