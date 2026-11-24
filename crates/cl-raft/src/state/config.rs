//! Cluster config entry handlers.

use super::*;

impl ClusterState {
    pub(super) fn apply_config(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::PutClusterConfig {
                yaml,
                expect_generation,
            } => {
                let current = self
                    .cluster_config
                    .as_ref()
                    .map(|c| c.generation)
                    .unwrap_or(0);
                if current != *expect_generation {
                    return Err(format!(
                        "cluster config was edited concurrently (stored generation {current}, edit was made against {expect_generation})"
                    ));
                }
                self.cluster_config = Some(ClusterConfigEntry {
                    yaml: yaml.clone(),
                    generation: current + 1,
                });
                Ok(())
            }

            Request::SetOverlayAllocations { allocations } => {
                self.overlay = Some(allocations.clone());
                Ok(())
            }

            _ => unreachable!("apply_config was handed a request routed elsewhere"),
        }
    }
}
