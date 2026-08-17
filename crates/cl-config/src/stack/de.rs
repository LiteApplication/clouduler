//! Serde helpers for compose fields that accept more than one shape on the wire
//! (a mapping or a `KEY=value` list, a single path or a list of paths).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};
use serde_yaml::Value;

/// `environment:`/`labels:`/`sysctls:` accept either a mapping or a list of
/// `KEY=value` strings. Both normalize to a plain string map.
pub(crate) fn map_or_list<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Map(BTreeMap<String, Value>),
        List(Vec<String>),
    }

    match Repr::deserialize(deserializer)? {
        Repr::Map(map) => Ok(map.into_iter().map(|(k, v)| (k, value_to_string(v))).collect()),
        Repr::List(list) => list
            .into_iter()
            .map(|entry| {
                entry
                    .split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!("expected KEY=value, got \"{entry}\""))
                    })
            })
            .collect(),
    }
}

fn value_to_string(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => serde_yaml::to_string(&other).unwrap_or_default().trim().to_string(),
    }
}

/// `env_file:` accepts a single path or a list of paths.
pub(crate) fn path_or_list<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        One(String),
        Many(Vec<String>),
    }

    Ok(match Repr::deserialize(deserializer)? {
        Repr::One(s) => vec![PathBuf::from(s)],
        Repr::Many(list) => list.into_iter().map(PathBuf::from).collect(),
    })
}
