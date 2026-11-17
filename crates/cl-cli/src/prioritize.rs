//! `clouduler prioritize`: puts a stack ahead of every other one until it is
//! reset (see `Request::SetPrioritized`).

use anyhow::{anyhow, bail, Result};
use cl_config::ClusterConfig;
use cl_raft::client::client_for;
use cl_raft::Request;

pub async fn run(
    cfg: &ClusterConfig,
    stack: Option<&str>,
    reset: bool,
    via: Option<&str>,
) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let (stacks, on): (Vec<String>, bool) = match (stack, reset) {
        (Some(stack), reset) => (vec![stack.to_string()], !reset),
        (None, true) => (state.prioritized.iter().cloned().collect(), false),
        (None, false) => bail!("name the stack to prioritize, or pass --reset"),
    };
    if stacks.is_empty() {
        println!("no stack is prioritized");
        return Ok(());
    }

    for stack in stacks {
        let response = client
            .submit(Request::SetPrioritized {
                stack: stack.clone(),
                on,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not change the priority of {stack}: {error}");
        }
        if on {
            println!("{stack} is prioritized until `clouduler prioritize --reset {stack}`");
        } else {
            println!("{stack} is back at its own priority");
        }
    }
    Ok(())
}
