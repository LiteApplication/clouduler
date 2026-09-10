use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use cl_cli::cli::{
    BackupCommand, Cli, Command, CompletionsCommand,
    ConfigCommand, DriveCommand, GitBackupCommand, NodeCommand, SecretCommand, SourceCommand,
    StackCommand,
};
use cl_init::detect;
use cl_config::ClusterConfig;
use tracing_subscriber::EnvFilter;

fn install_crypto_provider() {
    // Err means someone already installed one, which is just as good.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

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
        Command::Init(args) => return cl_init::run(*args, cli.config).await,
        Command::Completions {
            command: CompletionsCommand::Print { shell },
        } => return cl_cli::completions::print(shell),
        Command::Completions {
            command: CompletionsCommand::Install { shell },
        } => return cl_cli::completions::install(shell),
        Command::Daemon { node } => {
            let node = match node {
                Some(n) => n,
                None => hostname()?,
            };
            let cfg = ClusterConfig::load(&cli.config)?;
            return cl_daemon::daemon::run(cfg, node, cli.data_dir, cli.config).await;
        }
        _ => {}
    }

    let cfg = ClusterConfig::load(&cli.config)?;

    match cli.command {
        Command::Init(_) | Command::Completions { .. } | Command::Daemon { .. } => {
            unreachable!()
        }
        Command::Check => {
            cl_cli::report::summary(&cfg);
            Ok(())
        }
        Command::Config { command } => match command {
            ConfigCommand::Reload { via } => cl_cli::config::reload(&cfg, via.as_deref()).await,
        },
        Command::Node { command } => match command {
            NodeCommand::List => {
                cl_cli::report::nodes(&cfg);
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
                    cl_cli::cli::RoleArg::Manager => cl_raft::state::Role::Manager,
                    cl_cli::cli::RoleArg::Worker => cl_raft::state::Role::Worker,
                };
                cl_cli::node::add(&cfg, name, role, rpc_addr, &drive, &label, via.as_deref()).await
            }
            NodeCommand::Remove { name, via } => cl_cli::node::remove(&cfg, &name, via.as_deref()).await,
            NodeCommand::SetAvailability {
                name,
                availability,
                via,
            } => {
                cl_cli::node::set_availability(&cfg, &name, availability, via.as_deref()).await
            }
        },
        Command::Stack { command } => match command {
            StackCommand::Deploy { file, name, via } => {
                cl_cli::stack::deploy(&cfg, &file, name, via.as_deref()).await
            }
            StackCommand::List { via } => cl_cli::stack::list(&cfg, via.as_deref()).await,
            StackCommand::Show { name, files, via } => {
                cl_cli::stack::show(&cfg, &name, files, via.as_deref()).await
            }
            StackCommand::Remove { name, via } => {
                cl_cli::stack::remove(&cfg, &name, via.as_deref()).await
            }
            StackCommand::Stop { name, via } => {
                cl_cli::stack::stop(&cfg, &name, via.as_deref()).await
            }
            StackCommand::Start { name, via } => {
                cl_cli::stack::start(&cfg, &name, via.as_deref()).await
            }
            StackCommand::Restart { name, via } => {
                cl_cli::stack::restart(&cfg, &name, via.as_deref()).await
            }
            StackCommand::Logs { name, follow, tail, via } => {
                cl_cli::logs::merged(&cfg, &name, cl_cli::logs::LogsArgs { follow, tail }, via.as_deref()).await
            }
            StackCommand::Unhealthy {
                node,
                uptime,
                placement,
                logs,
                all,
                via,
            } => {
                let flags = if all {
                    cl_cli::health::ShowFlags {
                        node: true,
                        uptime: true,
                        placement: true,
                        logs: Some(logs.unwrap_or(10)),
                    }
                } else {
                    cl_cli::health::ShowFlags { node, uptime, placement, logs }
                };
                cl_cli::health::run(&cfg, via.as_deref(), flags).await
            }
            StackCommand::Source { name, action, via } => {
                match action.unwrap_or(SourceCommand::Get) {
                    SourceCommand::Get => {
                        cl_cli::stack::source_get(&cfg, &name, via.as_deref()).await
                    }
                    SourceCommand::Set { file } => {
                        cl_cli::stack::source_set(&cfg, &name, file.as_deref(), via.as_deref()).await
                    }
                }
            },
        },
        Command::Secret { command } => match command {
            SecretCommand::Set {
                name,
                value,
                file,
                no_trim,
                via,
            } => cl_cli::secret::set(&cfg, &name, value, file.as_deref(), no_trim, via.as_deref()).await,
            SecretCommand::List { via } => cl_cli::secret::list(&cfg, via.as_deref()).await,
        },
        Command::Drive { command } => match command {
            DriveCommand::List => {
                cl_cli::report::drives(&cfg);
                Ok(())
            }
        },
        Command::FolderImport(args) => {
            let node = match &args.node {
                Some(n) => n.clone(),
                None => detect::hostname()?,
            };
            cl_cli::folder_import::run(&cfg, &node, *args).await
        }
        Command::Emergency { stack, node, yes } => {
            let node = match &node {
                Some(n) => n.clone(),
                None => detect::hostname()?,
            };
            cl_cli::node::emergency(&cfg, &node, &stack, cli.data_dir, yes).await
        }
        Command::Restore {
            source,
            age_identity_file,
            age_identity_stdin,
            branch,
            node,
            yes,
        } => {
            cl_cli::restore::run(
                &cfg,
                cl_cli::restore::RestoreArgs {
                    name: node.as_deref().unwrap_or(&hostname()?),
                    source: &source,
                    age_identity_file: age_identity_file.as_deref(),
                    age_identity_stdin,
                    branch: &branch,
                    data_dir_override: cli.data_dir,
                    yes,
                },
            ).await
        }
        Command::Rebalance { via, yes } => cl_cli::rebalance::run(&cfg, via.as_deref(), yes).await,
        Command::Status => {
            cl_cli::report::summary(&cfg);
            Ok(())
        }
        Command::Logs {
            target,
            follow,
            tail,
            via,
        } => cl_cli::logs::run(&cfg, &target, cl_cli::logs::LogsArgs { follow, tail }, via.as_deref()).await,
        Command::Exec {
            target,
            user,
            via,
            cmd,
        } => cl_cli::exec::run(&cfg, &target, cmd, user, via.as_deref()).await,
        Command::Capture {
            target,
            network,
            write,
            via,
            filter,
        } => cl_cli::capture::run(&cfg, &target, network, write, filter, via.as_deref()).await,
        Command::Backup { command } => match *command {
            BackupCommand::Destination { command } => {
                cl_cli::backup_cli::destination(&cfg, command).await
            }
            BackupCommand::Target { command } => cl_cli::backup_cli::target(&cfg, command).await,
            BackupCommand::Run { id, via } => cl_cli::backup_cli::run(&cfg, &id, via.as_deref()).await,
            BackupCommand::History { target, via } => {
                cl_cli::backup_cli::history(&cfg, target.as_deref(), via.as_deref()).await
            }
            BackupCommand::Snapshots { destination, via } => {
                cl_cli::backup_cli::snapshots(&cfg, &destination, via.as_deref()).await
            }
            BackupCommand::RestoreDrive {
                drive,
                destination,
                snapshot,
                yes,
                via,
            } => {
                cl_cli::backup_cli::restore_drive(
                    &cfg,
                    &hostname()?,
                    &drive,
                    &destination,
                    snapshot.as_deref(),
                    yes,
                    via.as_deref(),
                ).await
            }
            BackupCommand::RestoreVolume {
                volume,
                destination,
                snapshot,
                yes,
                via,
            } => {
                cl_cli::backup_cli::restore_volume(
                    &cfg,
                    &hostname()?,
                    &volume,
                    &destination,
                    snapshot.as_deref(),
                    yes,
                    via.as_deref(),
                ).await
            }
            BackupCommand::RestoreService {
                stack,
                service,
                destination,
                snapshot,
                yes,
                via,
            } => {
                cl_cli::backup_cli::restore_service(
                    &cfg,
                    &hostname()?,
                    &stack,
                    &service,
                    destination.as_deref(),
                    snapshot.as_deref(),
                    yes,
                    via.as_deref(),
                ).await
            }
        },
        Command::GitBackup { command } => match command {
            GitBackupCommand::Pull { via } => cl_cli::git_backup_cli::pull(&cfg, via.as_deref()).await,
        },
    }
}

fn init_tracing(log: &str, verbose: bool) -> Result<()> {
    let filter = if verbose {
        format!("debug,{log}")
    } else {
        log.to_string()
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .init();
    Ok(())
}

fn hostname() -> Result<String> {
    detect::hostname()
}