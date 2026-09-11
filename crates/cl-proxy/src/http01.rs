//! In-memory store for ACME HTTP-01 challenge responses.
//!
//! Shared between the plain-http listener, which serves
//! `/.well-known/acme-challenge/<token>` out of it regardless of `Host`, and
//! the issuance code in `acme`, which populates it for the lifetime of one
//! authorization and removes the entry once the order moves past it.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct Http01Store {
    tokens: Mutex<HashMap<String, String>>,
}

impl Http01Store {
    pub fn set(&self, token: impl Into<String>, key_authorization: impl Into<String>) {
        self.tokens
            .lock()
            .expect("http01 store poisoned")
            .insert(token.into(), key_authorization.into());
    }

    pub fn get(&self, token: &str) -> Option<String> {
        self.tokens
            .lock()
            .expect("http01 store poisoned")
            .get(token)
            .cloned()
    }

    pub fn remove(&self, token: &str) {
        self.tokens
            .lock()
            .expect("http01 store poisoned")
            .remove(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_round_trips_until_removed() {
        let store = Http01Store::default();
        assert_eq!(store.get("tok"), None);
        store.set("tok", "tok.thumbprint");
        assert_eq!(store.get("tok").as_deref(), Some("tok.thumbprint"));
        store.remove("tok");
        assert_eq!(store.get("tok"), None);
    }
}
