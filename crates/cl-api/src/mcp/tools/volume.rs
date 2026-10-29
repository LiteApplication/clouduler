//! Volume freeze tools: pause a volume's replication so it can be edited in
//! place, then resume and push it.

use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::{Ctx, ToolResult};
use crate::volume_ops::{self, parse_volume, wait_of};

#[derive(Deserialize, JsonSchema)]
pub struct FreezeVolumeArgs {
    /// Volume as `drive/volume`.
    pub volume: String,
    /// Node whose copy will be edited. Defaults to the volume's primary.
    #[serde(default)]
    pub node: Option<String>,
    /// Stop containers using the volume on other nodes. Without it the call
    /// fails if there are any.
    #[serde(default)]
    pub force: bool,
    /// How long to wait for those containers to stop (default 120, max 600).
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UnfreezeVolumeArgs {
    /// Volume as `drive/volume`.
    pub volume: String,
    /// Start the containers the freeze stopped once every node has the
    /// edited copy. Default true.
    #[serde(default = "yes")]
    pub start_containers: bool,
    /// How long to wait for the other nodes to receive the volume (default
    /// 120, max 600). Containers are only started once they have it.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

fn yes() -> bool {
    true
}

pub(super) async fn freeze_volume(ctx: &Ctx, args: FreezeVolumeArgs) -> ToolResult {
    let volume = parse_volume(&args.volume)?;
    volume_ops::freeze(
        &ctx.state,
        &volume,
        args.node,
        args.force,
        Some(wait_of(args.wait_seconds)),
    )
    .await
}

pub(super) async fn unfreeze_volume(ctx: &Ctx, args: UnfreezeVolumeArgs) -> ToolResult {
    let volume = parse_volume(&args.volume)?;
    volume_ops::unfreeze(
        &ctx.state,
        &volume,
        args.start_containers,
        wait_of(args.wait_seconds),
    )
    .await
}
