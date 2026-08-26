mod cli;
mod config;
mod daemon;
mod drives;
mod folder_import;
mod internal_dns;
mod migration;
mod node;
mod placement;
mod overlay;
mod egress;
mod exec;
mod expose;
mod init;
mod logs;
mod proxy;
mod report;
mod secret;
mod stack;

use anyhow::{Context, Result};
use clap::Parser;
use cl_config::ClusterConfig;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, ConfigCommand, DriveCommand, NodeCommand, SecretCommand, StackCommand};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log, cli.verbose)?;
    install_crypto_provider();

    // Setting the node up is the one thing that runs without a config, since
    // writing that config is most of what it does.
    if let Command::Init(args) = cli.command {
        return init::run(*args, cli.config).await;
    }

    let cfg = ClusterConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Init(_) => unreachable!("handled before the config is loaded"),
        Command::Daemon { node } => {
            let node = match node {
                Some(n) => n,
                None => hostname()?,
            };
            daemon::run(cfg, node, cli.data_dir, cli.config).await
        }
        Command::Check => {
            report::summary(&cfg);
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Reload { via },
        } => config::reload(&cfg, via.as_deref()).await,
        Command::Node { command } => match command {
            NodeCommand::List => {
                report::nodes(&cfg);
                Ok(())
            }
            NodeCommand::Add {
                name,
                role,
                rpc_addr,
                drive,
                label,
                via,
            } => {
                let role = match role {
                    cli::RoleArg::Manager => cl_raft::state::Role::Manager,
                    cli::RoleArg::Worker => cl_raft::state::Role::Worker,
                };
                node::add(&cfg, name, role, rpc_addr, &drive, &label, via.as_deref()).await
            }
            NodeCommand::Remove { name, via } => node::remove(&cfg, &name, via.as_deref()).await,
            NodeCommand::SetAvailability { name, availability, via } => {
                node::set_availability(&cfg, &name, availability, via.as_deref()).await
            }
            NodeCommand::ForceQuorum { node: node_name, yes } => {
                let node_name = match node_name {
                    Some(n) => n,
                    None => hostname()?,
                };
                node::force_quorum(&cfg, &node_name, cli.data_dir, yes).await
            }
        },
        Command::Stack { command } => match command {
            StackCommand::Deploy { file, name, via } => {
                stack::deploy(&cfg, &file, name, via.as_deref()).await
            }
            StackCommand::List { via } => stack::list(&cfg, via.as_deref()).await,
            StackCommand::Show { name, files, via } => {
                stack::show(&cfg, &name, files, via.as_deref()).await
            }
            StackCommand::Remove { name, via } => stack::remove(&cfg, &name, via.as_deref()).await,
            StackCommand::Stop { name, via } => stack::stop(&cfg, &name, via.as_deref()).await,
            StackCommand::Start { name, via } => stack::start(&cfg, &name, via.as_deref()).await,
            StackCommand::Restart { name, via } => stack::restart(&cfg, &name, via.as_deref()).await,
            StackCommand::Logs { name, follow, tail, via } => {
                logs::merged(&cfg, &name, logs::LogsArgs { follow, tail }, via.as_deref()).await
            }
        },
        Command::Secret { command } => match command {
            SecretCommand::Set { name, value, file, via } => {
                secret::set(&cfg, &name, value, file.as_deref(), via.as_deref()).await
            }
            SecretCommand::List { via } => secret::list(&cfg, via.as_deref()).await,
        },
        Command::Drive {
            command: DriveCommand::List,
        } => {
            report::drives(&cfg);
            Ok(())
        }
        Command::FolderImport(args) => {
            let node = match &args.node {
                Some(n) => n.clone(),
                None => hostname()?,
            };
            folder_import::run(&cfg, &node, *args).await
        }
        Command::Status => {
            // Filled in once the raft layer can answer live queries.
            report::summary(&cfg);
            Ok(())
        }
        Command::Logs { target, follow, tail, via } => {
            logs::run(&cfg, &target, logs::LogsArgs { follow, tail }, via.as_deref()).await
        }
        Command::Exec { target, user, via, cmd } => {
            let cmd = if cmd.is_empty() { vec!["sh".to_string()] } else { cmd };
            exec::run(&cfg, &target, cmd, user, via.as_deref()).await
        }
    }
}

/// Pick the TLS backend once for the whole process.
///
/// rustls refuses to guess when more than one provider is compiled in, and it
/// refuses at the first handshake rather than at startup, which would mean the
/// web ui coming up and then dying the moment someone loaded it.
fn install_crypto_provider() {
    // Err means someone already installed one, which is just as good.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Own-crate debug directives `--verbose` layers on top of `--log`, so
/// `-v` shows what a command is doing without also turning on debug logging
/// for every dependency (openraft above all, which is noisy even at info).
const VERBOSE_DIRECTIVES: &str =
    "clouduler=debug,cl_drive=debug,cl_raft=debug,cl_sched=debug,cl_net=debug,cl_docker=debug,cl_proxy=debug,cl_dns=debug,cl_config=debug";

fn init_tracing(filter: &str, verbose: bool) -> Result<()> {
    let filter = if verbose {
        format!("{filter},{VERBOSE_DIRECTIVES}")
    } else {
        filter.to_string()
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&filter).context("invalid log filter")?)
        .with_target(false)
        .init();
    Ok(())
}

fn hostname() -> Result<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .context("reading hostname, pass --node instead")?;
    Ok(raw.trim().to_string())
}
