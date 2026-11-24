//! Config, secret and wrapped-key handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_secret(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::PutConfig {
                name,
                content,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(content).is_err() {
                    return Err(format!("config {name} is not valid base64"));
                }
                let revision = self.configs.get(name).map(|c| c.revision + 1).unwrap_or(1);
                let entry = ConfigEntry {
                    name: name.clone(),
                    content: content.clone(),
                    revision,
                    owner: owner.clone(),
                };
                if entry.len() > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "config {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.configs.insert(name.clone(), entry);
                Ok(())
            }

            Request::PutConfigCas {
                name,
                content,
                owner,
                expect_revision,
            } => {
                let current = self.configs.get(name).map(|c| c.revision).unwrap_or(0);
                if current != *expect_revision {
                    return Err(format!(
                        "config {name} changed (revision {current}, expected {expect_revision})"
                    ));
                }
                self.dispatch(&Request::PutConfig {
                    name: name.clone(),
                    content: content.clone(),
                    owner: owner.clone(),
                })
            }

            Request::RemoveConfig { name } => {
                if let Some(user) = self.config_in_use(name) {
                    return Err(format!("config {name} is still used by {user}"));
                }
                self.configs.remove(name);
                self.forget_output_object(&crate::terraform::OutputObject::config(name));
                Ok(())
            }

            Request::PutSecret {
                name,
                sealed,
                key_fingerprint,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(sealed).is_err() {
                    return Err(format!("secret {name} is not valid base64"));
                }
                let revision = self.secrets.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let entry = SecretEntry {
                    name: name.clone(),
                    sealed: sealed.clone(),
                    revision,
                    key_fingerprint: key_fingerprint.clone(),
                    owner: owner.clone(),
                };
                if entry.sealed.len() / 4 * 3 > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "secret {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.secrets.insert(name.clone(), entry);
                Ok(())
            }

            Request::RemoveSecret { name } => {
                if let Some(user) = self.secret_in_use(name) {
                    return Err(format!("secret {name} is still used by {user}"));
                }
                self.secrets.remove(name);
                self.forget_output_object(&crate::terraform::OutputObject::secret(name));
                Ok(())
            }

            Request::PutWrappedKey { node, entry } => {
                let current = self
                    .nodes
                    .get(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                if current.node_pubkey.as_deref() != Some(entry.target_pubkey.as_str()) {
                    return Err(format!(
                        "node {node} has not published the public key this was wrapped for"
                    ));
                }
                self.wrapped_keys.insert(node.clone(), entry.clone());
                Ok(())
            }

            _ => unreachable!("apply_secret was handed a request routed elsewhere"),
        }
    }
}
