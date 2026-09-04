mod backup_cli;
mod backup_restore;
mod backup_runner;
mod backup_secrets;
mod capture;
mod cli;
mod completion;
mod completions;
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
mod health;
mod init;
mod logs;
mod proxy;
mod rebalance;
mod report;
mod restore;
mod secret;
mod stack;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use cl_config::ClusterConfig;
use tracing_subscriber::EnvFilter;

use crate::cli::{
    BackupCommand, Cli, Command, CompletionsCommand, ConfigCommand, DriveCommand, NodeCommand, SecretCommand,
    SourceCommand, StackCommand,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Must run before anything else touches stdout: if `COMPLETE` is set in
    // the environment this answers a shell completion request (or, with no
    // further args, prints the registration snippet `completions print`
    // also prints) and exits, instead of falling through to the real CLI.
    // See `crate::completion` and `crate::completions`.
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();
    init_tracing(&cli.log, cli.verbose)?;
    install_crypto_provider();

    // Setting the node up is the one thing that runs without a config, since
    // writing that config is most of what it does. Printing or installing
    // completions needs no cluster at all.
    match cli.command {
        Command::Init(args) => return init::run(*args, cli.config).await,
        Command::Completions {
            command: CompletionsCommand::Print { shell },
        } => return completions::print(shell),
        Command::Completions {
            command: CompletionsCommand::Install { shell },
        } => return completions::install(shell),
        _ => {}
    }

    let cfg = ClusterConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.command {
        Command::Init(_) | Command::Completions { .. } => {
            unreachable!("handled before the config is loaded")
        }
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
        },
        Command::Emergency { stack, node: node_name, yes } => {
            let node_name = match node_name {
                Some(n) => n,
                None => hostname()?,
            };
            node::emergency(&cfg, &node_name, &stack, cli.data_dir, yes).await
        }
        Command::Restore {
            source,
            age_identity_file,
            age_identity_stdin,
            branch,
            node: node_name,
            yes,
        } => {
            let node_name = match node_name {
                Some(n) => n,
                None => hostname()?,
            };
            restore::run(
                &cfg,
                restore::RestoreArgs {
                    name: &node_name,
                    source: &source,
                    age_identity_file: age_identity_file.as_deref(),
                    age_identity_stdin,
                    branch: &branch,
                    data_dir_override: cli.data_dir,
                    yes,
                },
            )
            .await
        }
        Command::Rebalance { via, yes } => rebalance::run(&cfg, via.as_deref(), yes).await,
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
            StackCommand::Unhealthy { node, uptime, placement, logs, all, via } => {
                let flags = if all {
                    health::ShowFlags { node: true, uptime: true, placement: true, logs: Some(logs.unwrap_or(10)) }
                } else {
                    health::ShowFlags { node, uptime, placement, logs }
                };
                health::run(&cfg, via.as_deref(), flags).await
            }
            StackCommand::Source { name, action, via } => match action.unwrap_or(SourceCommand::Get) {
                SourceCommand::Get => stack::source_get(&cfg, &name, via.as_deref()).await,
                SourceCommand::Set { file } => stack::source_set(&cfg, &name, file.as_deref(), via.as_deref()).await,
            },
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
        Command::Capture { target, network, write, via, filter } => {
            capture::run(&cfg, &target, network, write, filter, via.as_deref()).await
        }
        Command::Backup { command } => match *command {
            BackupCommand::Destination { command } => backup_cli::destination(&cfg, command).await,
            BackupCommand::Target { command } => backup_cli::target(&cfg, command).await,
            BackupCommand::Run { id, via } => backup_cli::run(&cfg, &id, via.as_deref()).await,
            BackupCommand::History { target, via } => backup_cli::history(&cfg, target.as_deref(), via.as_deref()).await,
            BackupCommand::Snapshots { destination, via } => backup_cli::snapshots(&cfg, &destination, via.as_deref()).await,
            BackupCommand::RestoreDrive { drive, destination, snapshot, yes, via } => {
                let node_name = hostname()?;
                backup_cli::restore_drive(&cfg, &node_name, &drive, &destination, snapshot.as_deref(), yes, via.as_deref()).await
            }
            BackupCommand::RestoreVolume { volume, destination, snapshot, yes, via } => {
                let node_name = hostname()?;
                backup_cli::restore_volume(&cfg, &node_name, &volume, &destination, snapshot.as_deref(), yes, via.as_deref()).await
            }
            BackupCommand::RestoreService { stack, service, destination, snapshot, yes, via } => {
                let node_name = hostname()?;
                backup_cli::restore_service(&cfg, &node_name, &stack, &service, destination.as_deref(), snapshot.as_deref(), yes, via.as_deref()).await
            }
        },
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
