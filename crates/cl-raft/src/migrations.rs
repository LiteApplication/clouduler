//! One-time transforms applied to a persisted `StateMachine` blob when it was
//! written by an older schema version than this build understands.
//!
//! This is the other half of the compatibility story alongside `Request`'s
//! forward-tolerant `Deserialize` (`state.rs`): that lets an *old* node
//! ignore data from a *new* node without crashing during a rolling deploy;
//! this lets a *new* node read data written by an *old* node without
//! permanent, scattered backward-compat shims on the live types. A shape
//! change lands here once, as a plain JSON transform, and the live types in
//! `state.rs` only ever need to understand the current shape.
//!
//! Runs wherever a `StateMachine` blob is loaded from disk or from a peer's
//! snapshot (see `store.rs`): parse to `serde_json::Value`, read its
//! `schema_version` (defaulting to `0` for data written before this field
//! existed), run every migration from that version up to
//! `state::CURRENT_SCHEMA_VERSION`, then decode the result into `StateMachine`
//! normally.

use serde_json::Value;

/// A pure transform from one schema version's shape to the next. Operates on
/// the whole `StateMachine` JSON value (not just `data`/`ClusterState`) so a
/// migration can reach anything, including `schema_version` itself if it
/// ever needs to (ordinarily left to the caller to bump).
pub type Migration = fn(Value) -> Value;

/// Index `n` migrates schema_version `n` to `n + 1`. Append here, never edit
/// or remove an existing entry - each one must keep working for as long as
/// any node in the field might still be running data at that version.
pub const MIGRATIONS: &[Migration] = &[];

/// Runs every migration needed to bring `value` from `from_version` up to
/// `state::CURRENT_SCHEMA_VERSION`. A no-op when already current (or, in
/// principle, unreachable when newer than current - callers are expected to
/// have already refused that case before calling this, see `store.rs`).
pub fn migrate_to_current(mut value: Value, from_version: u32) -> Value {
    for migration in MIGRATIONS.iter().skip(from_version as usize) {
        value = migration(value);
    }
    // Stamp the version explicitly rather than relying on the last migration
    // to do it (there may be none to run, e.g. a brand new node's first
    // write), so anything read back later always carries an accurate,
    // current `schema_version`.
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "schema_version".to_string(),
            serde_json::json!(crate::state::CURRENT_SCHEMA_VERSION),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrating_from_the_current_version_is_a_no_op() {
        let value = serde_json::json!({"schema_version": crate::state::CURRENT_SCHEMA_VERSION});
        let migrated = migrate_to_current(value.clone(), crate::state::CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated, value);
    }
}
