//! Errors for the overlay, IPAM and internal DNS.

pub type Result<T> = std::result::Result<T, NetError>;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// A netlink request failed. Carries the interface name so the operator
    /// knows which device to look at.
    #[error("netlink operation on {interface} failed: {source}")]
    Netlink {
        interface: String,
        #[source]
        source: rtnetlink::Error,
    },

    /// Bridge, vxlan and fdb changes need CAP_NET_ADMIN. Say so plainly rather
    /// than surfacing the raw EPERM.
    #[error(
        "permission denied managing {interface}: overlay networking needs \
         CAP_NET_ADMIN (run clouduler as root or grant the capability)"
    )]
    PermissionDenied { interface: String },

    #[error("no free address left in subnet {subnet} for network {network}")]
    AddressExhausted { network: String, subnet: String },

    #[error("no free subnet left in overlay pool {pool}")]
    SubnetExhausted { pool: String },

    /// A candidate overlay subnet collides with a subnet already claimed by
    /// the cluster config (the internal network or a private network).
    #[error("subnet {subnet} for network {network} overlaps cluster network {conflicts}")]
    SubnetOverlap {
        network: String,
        subnet: String,
        conflicts: String,
    },

    #[error("unknown network {0}")]
    UnknownNetwork(String),

    #[error("dns error: {0}")]
    Dns(String),

    #[error("io error on {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl NetError {
/// Map a raw netlink error, recognising EPERM/EACCES as the CAP_NET_ADMIN message.
    pub fn from_netlink(interface: impl Into<String>, source: rtnetlink::Error) -> Self {
        let interface = interface.into();
        if is_permission_error(&source) {
            NetError::PermissionDenied { interface }
        } else {
            NetError::Netlink { interface, source }
        }
    }
}

fn is_permission_error(err: &rtnetlink::Error) -> bool {
    match err {
        rtnetlink::Error::NetlinkError(msg) => {
            let code = msg.code.map(|c| c.get()).unwrap_or(0);
            code == -1 || code == -13 // EPERM, EACCES
        }
        _ => false,
    }
}
