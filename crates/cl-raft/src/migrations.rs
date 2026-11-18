//! One-time transforms applied to a persisted `StateMachine` blob written by an
//! older schema version. Complements `Request`'s forward-tolerant `Deserialize`
//! (`state.rs`): that lets old nodes ignore new data during a rolling deploy;
//! this lets new nodes read old data without backward-compat shims on the live
//! types. Applied wherever a blob is loaded from disk or a peer snapshot
//! (`store.rs`): parse to JSON, read `schema_version` (0 if absent), run
//! migrations up to `state::CURRENT_SCHEMA_VERSION`, then decode normally.

use serde_json::Value;

/// A pure transform from one schema version's shape to the next, over the whole
/// `StateMachine` JSON value.
pub type Migration = fn(Value) -> Value;

/// Index `n` migrates schema_version `n` to `n + 1`. Append here, never edit
/// or remove an existing entry - each one must keep working for as long as
/// any node in the field might still be running data at that version.
pub const MIGRATIONS: &[Migration] = &[|v| v, emergency_stack_to_set];

/// Version 1 held a single `emergency_stack` name (or null); version 2 holds
/// the set `emergency_stacks`.
fn emergency_stack_to_set(mut value: Value) -> Value {
    if let Some(data) = value.get_mut("data").and_then(Value::as_object_mut) {
        let old = data.remove("emergency_stack");
        let stacks = match old {
            Some(Value::String(name)) => vec![Value::String(name)],
            _ => Vec::new(),
        };
        data.insert("emergency_stacks".into(), Value::Array(stacks));
    }
    value
}

/// Brings `value` from `from_version` up to `state::CURRENT_SCHEMA_VERSION`.
/// Callers must already have refused newer-than-current data (`store.rs`).
pub fn migrate_to_current(mut value: Value, from_version: u32) -> Value {
    for migration in MIGRATIONS.iter().skip(from_version as usize) {
        value = migration(value);
    }
    // Stamp explicitly: there may be no migration to run (e.g. a new node's first write).
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
    fn a_single_emergency_stack_becomes_a_set() {
        let old = serde_json::json!({"schema_version": 1, "data": {"emergency_stack": "site"}});
        let new = migrate_to_current(old, 1);
        assert_eq!(new["data"]["emergency_stacks"], serde_json::json!(["site"]));
        assert!(new["data"].get("emergency_stack").is_none());
    }

    #[test]
    fn migrating_from_the_current_version_is_a_no_op() {
        let value = serde_json::json!({"schema_version": crate::state::CURRENT_SCHEMA_VERSION});
        let migrated = migrate_to_current(value.clone(), crate::state::CURRENT_SCHEMA_VERSION);
        assert_eq!(migrated, value);
    }
}
