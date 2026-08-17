//! Encryption for the secrets held in the cluster database.
//!
//! Secrets are sealed before they enter the database, so the replicated state,
//! its snapshots and any backup of them are safe to copy around. The key stays
//! out of the database entirely: it lives in a file on at least one node, and a
//! node without it replicates sealed secrets happily but cannot open them.
//!
//! XChaCha20-Poly1305 with a fresh random nonce per seal. The nonce is 24 bytes,
//! large enough that random generation needs no counter and no coordination
//! between nodes, which matters because any node may seal a secret.

mod error;
mod key;
mod seal;

pub use error::{Result, SecretError};
pub use key::{ClusterKey, KEY_BYTES};
pub use seal::{Sealed, NONCE_BYTES};
