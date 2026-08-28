//! Docker engine access. Everything that talks to the local daemon goes here.
//! Wraps bollard with clouduler's own container spec/state types and the
//! ownership labels that distinguish clouduler's containers from the user's.

mod client;
mod container;
mod error;
mod events;
mod exec;
mod image;
mod logs;
mod network;

pub use client::Docker;
pub use container::{
    BindMount, ContainerSpec, ContainerState, HealthState, Healthcheck, PortMapping, PortProtocol,
    ResourceLimits, RestartPolicy, RunState, TmpfsMount, LABEL_INSTANCE, LABEL_MANAGED,
    LABEL_SERVICE, LABEL_STACK, MANAGED_VALUE,
};
pub use error::{DockerError, Result};
pub use events::ContainerEvent;
pub use exec::{ExecChunk, ExecSession, ExecSpec, ExecStream};
pub use image::{ImageRef, RegistryCredentials, DOCKER_HUB};
pub use logs::{LogLine, LogOptions, LogStream, Tail};
pub use network::NetworkSpec;
