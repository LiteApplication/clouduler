//! Image update tools: look at the registries again, then update what is behind.

use std::fmt::Write;

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::{parse_instance, Ctx, NoArgs, ToolResult};
use crate::routes::updates::{self, InstanceSelection};

#[derive(Deserialize, JsonSchema)]
pub struct UpdateBehindArgs {
    /// Only this service of the focused stack (every replica that is behind).
    #[serde(default)]
    pub service: Option<String>,
    /// Only these instance ids (`stack.service.N`), all in the focused stack. Combines with `service` as an intersection.
    #[serde(default)]
    pub instances: Vec<String>,
}

fn describe(behind: &[updates::BehindInstance]) -> String {
    let mut out = String::new();
    for b in behind {
        let _ = writeln!(out, "{}  image={}", b.id, b.image.as_deref().unwrap_or("?"));
    }
    out
}

/// Cluster-wide: a registry lookup is shared by every instance using the image, so it is not per stack.
pub(super) async fn refresh_updates(ctx: &Ctx, _args: NoArgs) -> ToolResult {
    updates::check_images(&ctx.state).await;
    let behind = updates::behind(&ctx.state).await;
    if behind.is_empty() {
        return Ok("registries checked; no instance is behind its image".to_string());
    }
    Ok(format!(
        "registries checked; {} instance(s) behind (cluster-wide):\n{}Use update_behind to update the ones in the focused stack.",
        behind.len(),
        describe(&behind)
    ))
}

/// Limited to the focused stack, like every other write.
pub(super) async fn update_behind(ctx: &Ctx, args: UpdateBehindArgs) -> ToolResult {
    let stack = ctx.require_focus()?;
    let mut wanted = Vec::new();
    for raw in &args.instances {
        let id = parse_instance(raw)?;
        ctx.require_in_focus(&id.stack, &format!("instance {id}"))?;
        wanted.push(id);
    }
    let behind: Vec<_> = updates::behind(&ctx.state)
        .await
        .into_iter()
        .filter(|b| b.id.stack == stack)
        .filter(|b| args.service.as_ref().is_none_or(|s| &b.id.service == s))
        .filter(|b| wanted.is_empty() || wanted.contains(&b.id))
        .collect();
    if behind.is_empty() {
        return Ok(format!(
            "no matching instance of {stack} is behind its image (as of the last registry check; run refresh_updates to look again)"
        ));
    }
    let selection = InstanceSelection {
        instances: behind.iter().map(|b| b.id.to_string()).collect(),
    };
    let (_, axum::Json(accepted)) = updates::apply_selection(&ctx.state, selection).await;
    let started = accepted.accepted();
    Ok(format!(
        "updating {} instance(s) of {stack}, one restart per node at a time; the first failure stops the batch and rolls that instance back. Check list_problems in a few seconds:\n{}",
        started.len(),
        started.join("\n")
    ))
}
