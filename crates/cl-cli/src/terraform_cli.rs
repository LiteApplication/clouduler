//! `clouduler terraform plan|apply|status`: manual runs and the last run of a stack's OpenTofu modules.
//!
//! Plan and apply only record a request in raft; the runner on the node hosting the target instance
//! claims it on its next pass (the wake-up is in-process, so the CLI cannot nudge it).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_raft::client::client_for;
use cl_raft::state::MAX_OBJECT_BYTES;
use cl_raft::terraform::{terraform_key, TerraformMode, TerraformOutcome};
use cl_raft::Request;

/// Reads `terraform/<service>.tf` next to a stack file for every service that has one. `None` when
/// the directory does not exist, so a deploy without it leaves the stored modules alone. Each module
/// is checked with the same validation the cluster API applies.
pub fn load_modules(base: &Path, parsed: &StackFile) -> Result<Option<BTreeMap<String, String>>> {
    let dir = base.join("terraform");
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut modules = BTreeMap::new();
    for service in parsed.services.keys() {
        let path = dir.join(format!("{service}.tf"));
        if !path.is_file() {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if source.len() > MAX_OBJECT_BYTES {
            bail!(
                "{} is over the {MAX_OBJECT_BYTES} byte limit",
                path.display()
            );
        }
        cl_config::validate_module(&source).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        modules.insert(service.clone(), source);
    }
    Ok(Some(modules))
}

pub async fn run(
    cfg: &ClusterConfig,
    apply: bool,
    stack: &str,
    service: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    if cfg.terraform.is_none() {
        bail!("terraform is not enabled: add a `terraform:` section to the cluster config");
    }
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let services = services_of(&state, stack, service)?;
    let mode = if apply {
        TerraformMode::Apply
    } else {
        TerraformMode::Plan
    };
    let verb = match mode {
        TerraformMode::Plan => "plan",
        TerraformMode::Apply => "apply",
    };
    for service in services {
        let response = client
            .submit(Request::RequestTerraformRun {
                key: terraform_key(stack, &service),
                mode,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not request {verb} of {stack}/{service}: {error}");
        }
        println!("requested {verb} of {stack}/{service}; see `clouduler terraform status {stack}`");
    }
    Ok(())
}

pub async fn status(
    cfg: &ClusterConfig,
    stack: &str,
    service: Option<&str>,
    via: Option<&str>,
) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let services = services_of(&state, stack, service)?;
    if cfg.terraform.is_none() {
        println!("terraform is not enabled in the cluster config; nothing will run");
    }
    for service in services {
        let key = terraform_key(stack, &service);
        let revision = state.terraform_modules[stack][&service].revision;
        println!("{key}: module revision {revision}");
        if let Some(mode) = state.terraform_requests.get(&key) {
            println!("  pending request: {mode:?}");
        }
        match state.terraform_runs.get(&key) {
            None => println!("  never run"),
            Some(run) => {
                let outcome = match &run.outcome {
                    TerraformOutcome::Running => "running".to_string(),
                    TerraformOutcome::Succeeded { summary } => {
                        format!("succeeded: {}", summary.trim())
                    }
                    TerraformOutcome::Failed { error } => format!("FAILED: {}", error.trim()),
                };
                println!(
                    "  last run: {:?} ({}) on {} at {}: {outcome}",
                    run.mode, run.trigger, run.node, run.started_at
                );
                if matches!(run.outcome, TerraformOutcome::Failed { .. })
                    && !run.log_tail.is_empty()
                {
                    println!("{}", run.log_tail.trim_end());
                }
            }
        }
    }
    Ok(())
}

/// The services of `stack` that have a module, or just `service` when named.
fn services_of(
    state: &cl_raft::state::ClusterState,
    stack: &str,
    service: Option<&str>,
) -> Result<Vec<String>> {
    if !state.stacks.contains_key(stack) {
        bail!("no stack named {stack}");
    }
    let modules = state
        .terraform_modules
        .get(stack)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| anyhow!("stack {stack} has no terraform modules"))?;
    match service {
        Some(s) if modules.contains_key(s) => Ok(vec![s.to_string()]),
        Some(s) => bail!("service {s} of {stack} has no terraform module"),
        None => Ok(modules.keys().cloned().collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed() -> StackFile {
        StackFile::parse("services:\n  web:\n    image: x\n  db:\n    image: y\n").unwrap()
    }

    #[test]
    fn no_terraform_dir_means_keep_existing_modules() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_modules(dir.path(), &parsed()).unwrap(), None);
    }

    #[test]
    fn picks_up_a_module_per_service_and_ignores_strays() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("terraform")).unwrap();
        std::fs::write(dir.path().join("terraform/web.tf"), "variable \"x\" {}\n").unwrap();
        std::fs::write(dir.path().join("terraform/ghost.tf"), "variable \"z\" {}\n").unwrap();
        let modules = load_modules(dir.path(), &parsed()).unwrap().unwrap();
        assert_eq!(modules.keys().collect::<Vec<_>>(), vec!["web"]);
    }

    #[test]
    fn a_backend_block_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("terraform")).unwrap();
        std::fs::write(
            dir.path().join("terraform/web.tf"),
            "terraform {\n  backend \"s3\" {}\n}\n",
        )
        .unwrap();
        assert!(load_modules(dir.path(), &parsed()).is_err());
    }
}
