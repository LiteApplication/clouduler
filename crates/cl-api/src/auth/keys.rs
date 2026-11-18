//! API key authentication.
//!
//! cl-raft has no `Request` variant for api keys, so they are stored as an ordinary config object under the
//! reserved name [`API_KEYS_CONFIG`]; routes that list or serve user configs must filter it out.
//!
//! A key looks like `clk_<id>_<secret>`. Only a SHA-256 hash of the secret is stored; the plaintext is shown once,
//! at creation. Every key is admin: the api key path is the way into a fresh cluster with no OIDC, so it must do everything.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use cl_raft::ClusterState;

/// Never exposed through `/api/configs`.
pub const API_KEYS_CONFIG: &str = "_clouduler_api_keys";

/// Makes a stray token recognizable in a log line.
const KEY_PREFIX: &str = "clk";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub name: Option<String>,
    pub hash: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiKeyStore {
    pub keys: Vec<ApiKeyRecord>,
}

impl ApiKeyStore {
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(API_KEYS_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        crate::encode_json_config(self)
    }

    pub fn find(&self, id: &str) -> Option<&ApiKeyRecord> {
        self.keys.iter().find(|k| k.id == id)
    }
}

pub struct NewKey {
    pub record: ApiKeyRecord,
    pub secret: String,
}

pub fn generate(name: Option<String>, created_at: i64) -> NewKey {
    let id = random_token(12);
    let secret = random_token(32);
    let record = ApiKeyRecord {
        id: id.clone(),
        name,
        hash: hash_secret(&secret),
        created_at,
    };
    NewKey {
        record,
        secret: format!("{KEY_PREFIX}_{id}_{secret}"),
    }
}

pub(crate) fn random_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let mut buf = [0u8; 32];
        rand::fill(&mut buf);
        for byte in buf {
            if out.len() == len {
                break;
            }
            if (byte as usize) < (256 / ALPHABET.len()) * ALPHABET.len() {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    out
}

pub fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(KEY_PREFIX)?.strip_prefix('_')?;
    rest.split_once('_')
}

/// The hash comparison is constant time, so timing cannot narrow down a valid hash byte by byte.
pub fn verify<'a>(store: &'a ApiKeyStore, id: &str, secret: &str) -> Option<&'a ApiKeyRecord> {
    let record = store.find(id)?;
    let expected = record.hash.as_bytes();
    let actual = hash_secret(secret);
    let actual = actual.as_bytes();
    if expected.len() != actual.len() {
        return None;
    }
    let diff = expected
        .iter()
        .zip(actual)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    (diff == 0).then_some(record)
}

pub fn parse_authorization_header(value: &str) -> Option<(&str, &str)> {
    let token = value.strip_prefix("Bearer ").unwrap_or(value);
    parse(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::base64_decode;

    #[test]
    fn a_generated_key_verifies_against_its_own_record() {
        let new_key = generate(Some("ci".into()), 0);
        let store = ApiKeyStore {
            keys: vec![new_key.record.clone()],
        };
        let (id, secret) = parse(&new_key.secret).expect("parses");
        assert_eq!(id, new_key.record.id);
        assert!(verify(&store, id, secret).is_some());
    }

    #[test]
    fn a_wrong_secret_does_not_validate() {
        let new_key = generate(None, 0);
        let store = ApiKeyStore {
            keys: vec![new_key.record.clone()],
        };
        assert!(verify(&store, &new_key.record.id, "not-the-secret").is_none());
    }

    #[test]
    fn a_wrong_id_does_not_validate() {
        let new_key = generate(None, 0);
        let store = ApiKeyStore {
            keys: vec![new_key.record],
        };
        assert!(verify(&store, "ghost", "whatever").is_none());
    }

    #[test]
    fn malformed_tokens_do_not_parse() {
        assert!(parse("not-a-key").is_none());
        assert!(parse("clk_onlyid").is_none());
        assert!(parse("wrongprefix_id_secret").is_none());
    }

    #[test]
    fn the_authorization_header_strips_the_bearer_prefix() {
        let new_key = generate(None, 0);
        let header = format!("Bearer {}", new_key.secret);
        let (id, secret) = parse_authorization_header(&header).expect("parses");
        assert_eq!(id, new_key.record.id);
        assert_eq!(secret, new_key.secret.rsplit('_').next().unwrap());
    }

    #[test]
    fn the_store_round_trips_through_config_encoding() {
        let new_key = generate(Some("laptop".into()), 42);
        let store = ApiKeyStore {
            keys: vec![new_key.record],
        };
        let encoded = store.encode();
        let raw = base64_decode(&encoded).expect("valid base64");
        let decoded: ApiKeyStore = serde_json::from_slice(&raw).expect("valid json");
        assert_eq!(decoded.keys.len(), 1);
        assert_eq!(decoded.keys[0].name.as_deref(), Some("laptop"));
    }

    #[test]
    fn generated_secrets_differ() {
        let a = generate(None, 0);
        let b = generate(None, 0);
        assert_ne!(a.secret, b.secret);
    }
}
