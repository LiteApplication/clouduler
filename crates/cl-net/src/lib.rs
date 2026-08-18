//! Overlay networking: VXLAN links, IPAM and the internal DNS resolver.
//!
//! Trust boundary: this crate builds the container overlay on top of the
//! cluster's single `internal` network (see `cl_config::NetworkKind::Internal`).
//! That network is explicitly unencrypted, on the assumption that the operator
//! treats it as trusted (a private LAN, a WireGuard/Tailscale mesh, etc). VXLAN
//! traffic carried here inherits that assumption: nothing in this crate
//! encrypts or authenticates overlay packets. Anyone with access to the
//! internal network can read or spoof container traffic. Don't run the
//! internal network over anything you don't already trust.

pub mod dns;
pub mod error;
pub mod expose;
pub mod ipam;
pub mod vxlan;

pub use error::{NetError, Result};
pub use ipam::Allocations;
pub use vxlan::{Overlay, Underlay};
