//! Cloudflare DNS: API client and the reconciler that owns clouduler records.

mod acme;
mod api;
mod desired;
mod error;
mod ownership;
mod reconcile;

pub use acme::{await_propagation, clear_challenge, set_challenge};
pub use api::{Cloudflare, Record, RecordInput, RecordType};
pub use desired::{records_for, web_records_for, DesiredRecord};
pub use error::{DnsError, Result, CF_ERR_DUPLICATE, CF_ERR_RECORD_EXISTS};
pub use ownership::{is_owned_by, parse_tag, record_key, tag_content, tag_name, Tag};
pub use reconcile::{apply, normalize_desired, plan, Plan};
