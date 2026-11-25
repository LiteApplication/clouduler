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

/// The oldest schema version this build can still read. Older blobs predate
/// the first entry of [`MIGRATIONS`] and are refused: such a node must first
/// be upgraded through a release that still carried the migration.
pub const OLDEST_SUPPORTED_VERSION: u32 = 2;

/// Entry `n` migrates schema_version `OLDEST_SUPPORTED_VERSION + n` to the
/// next one, so the list holds one entry per version above the oldest
/// supported. Append here when bumping `state::CURRENT_SCHEMA_VERSION`, never
/// edit an existing entry - each one must keep working for as long as any node
/// in the field might still be running data at that version.
pub const MIGRATIONS: &[Migration] = &[];

/// Brings `value` from `from_version` up to `state::CURRENT_SCHEMA_VERSION`.
/// Callers must already have refused newer-than-current data (`store.rs`).
pub fn migrate_to_current(value: Value, from_version: u32) -> Result<Value, String> {
    migrate_with(MIGRATIONS, value, from_version)
}

fn migrate_with(
    migrations: &[Migration],
    mut value: Value,
    from_version: u32,
) -> Result<Value, String> {
    if from_version < OLDEST_SUPPORTED_VERSION {
        return Err(format!(
            "state is schema_version {from_version}, older than the oldest this build can migrate ({OLDEST_SUPPORTED_VERSION}); upgrade this node through an intermediate release first"
        ));
    }
    let skip = (from_version - OLDEST_SUPPORTED_VERSION) as usize;
    for migration in migrations.iter().skip(skip) {
        value = migration(value);
    }
    // Stamp explicitly: there may be no migration to run (e.g. a new node's first write).
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "schema_version".to_string(),
            serde_json::json!(crate::state::CURRENT_SCHEMA_VERSION),
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_migration_list_reaches_the_current_version() {
        assert_eq!(
            OLDEST_SUPPORTED_VERSION + MIGRATIONS.len() as u32,
            crate::state::CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn migrating_from_the_current_version_is_a_no_op() {
        let value = serde_json::json!({"schema_version": crate::state::CURRENT_SCHEMA_VERSION});
        let migrated =
            migrate_to_current(value.clone(), crate::state::CURRENT_SCHEMA_VERSION).unwrap();
        assert_eq!(migrated, value);
    }

    #[test]
    fn a_blob_older_than_the_oldest_supported_version_is_refused() {
        let err =
            migrate_to_current(serde_json::json!({}), OLDEST_SUPPORTED_VERSION - 1).unwrap_err();
        assert!(err.contains("older than the oldest"), "{err}");
    }

    #[test]
    fn migrations_run_from_the_blobs_version_onwards() {
        fn first(mut v: Value) -> Value {
            v["steps"] = serde_json::json!([1]);
            v
        }
        fn second(mut v: Value) -> Value {
            v["steps"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(2));
            v
        }
        let list: &[Migration] = &[first, second];
        let from_oldest =
            migrate_with(list, serde_json::json!({}), OLDEST_SUPPORTED_VERSION).unwrap();
        assert_eq!(from_oldest["steps"], serde_json::json!([1, 2]));
        let from_next = migrate_with(
            list,
            serde_json::json!({"steps": [1]}),
            OLDEST_SUPPORTED_VERSION + 1,
        )
        .unwrap();
        assert_eq!(from_next["steps"], serde_json::json!([1, 2]));
    }
}
