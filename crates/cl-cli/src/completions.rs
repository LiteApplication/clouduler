//! `clouduler completions print|install`.
//! Live completion uses clap_complete's `unstable-dynamic` `CompleteEnv` engine
//! (wired up in `main`), which re-execs `clouduler` with `COMPLETE=<shell>` to print
//! a registration snippet or answer a request, so "installing" just means getting
//! that snippet sourced by the shell.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::ShellArg;
use cl_init::permissions;

impl ShellArg {
    fn name(self) -> &'static str {
        match self {
            ShellArg::Bash => "bash",
            ShellArg::Zsh => "zsh",
            ShellArg::Fish => "fish",
            ShellArg::Elvish => "elvish",
            ShellArg::Powershell => "powershell",
        }
    }
}

/// The line to add to the shell startup so it sources `COMPLETE=<shell> clouduler`
/// (piped for fish) on every new shell.
fn registration_line(shell: ShellArg, bin: &str) -> String {
    match shell {
        ShellArg::Bash => format!("source <(COMPLETE=bash {bin})"),
        ShellArg::Zsh => format!("source <(COMPLETE=zsh {bin})"),
        ShellArg::Fish => format!("COMPLETE=fish {bin} | source"),
        ShellArg::Elvish => format!("eval (E:COMPLETE=elvish {bin} | slurp)"),
        ShellArg::Powershell => format!(
            "$env:COMPLETE = \"powershell\"; {bin} | Out-String | Invoke-Expression; Remove-Item Env:\\COMPLETE"
        ),
    }
}

pub fn print(shell: ShellArg) -> Result<()> {
    let bin = bin_name()?;
    println!("{}", registration_line(shell, &bin));
    Ok(())
}

/// Detect the shell from `$SHELL` (`/bin/zsh` -> zsh) when none was given.
fn detect_shell() -> Result<ShellArg> {
    let shell = std::env::var("SHELL")
        .context("cannot detect a shell: $SHELL is not set; pass one explicitly")?;
    let name = shell.rsplit('/').next().unwrap_or(&shell);
    match name {
        "bash" => Ok(ShellArg::Bash),
        "zsh" => Ok(ShellArg::Zsh),
        "fish" => Ok(ShellArg::Fish),
        "elvish" => Ok(ShellArg::Elvish),
        _ => bail!("cannot tell a completion shell from $SHELL={shell}; pass one explicitly"),
    }
}

fn bin_name() -> Result<String> {
    Ok(std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "clouduler".to_string()))
}

/// The dynamic completers read `cluster.yaml`, which is group-readable by `clouduler`
/// (see `init::permissions::ensure`). Check now that the user can read it, since
/// otherwise the install looks fine until completion silently returns nothing.
fn warn_if_not_in_group() {
    match permissions::current_user_is_member() {
        Some(true) => {}
        Some(false) => {
            println!(
                "warning: this user is not in the `{group}` group, so live completions \
                 (stack, node, instance and drive names) will silently come back empty. \
                 Run `sudo usermod -aG {group} $(whoami)` and log back in to fix that; \
                 subcommand and flag completion will still work either way.",
                group = permissions::GROUP,
            );
        }
        None => {
            println!(
                "note: the `{group}` group does not exist on this machine yet - it is created \
                 the first time a node's daemon starts here. Once it has, add this user with \
                 `sudo usermod -aG {group} $(whoami)` and log back in to get live completions \
                 (stack, node, instance and drive names); subcommand and flag completion will \
                 still work either way.",
                group = permissions::GROUP,
            );
        }
    }
}

/// Where each shell keeps a rc file this appends to, or None when the
/// idiomatic move is a dedicated completion file instead (fish).
fn rc_file(shell: ShellArg) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot install completions: $HOME is not set"))?;
    Ok(match shell {
        ShellArg::Bash => home.join(".bashrc"),
        ShellArg::Zsh => home.join(".zshrc"),
        ShellArg::Elvish => home.join(".elvish").join("rc.elv"),
        ShellArg::Fish => home.join(".config").join("fish").join("completions").join("clouduler.fish"),
        ShellArg::Powershell => bail!(
            "installing into a PowerShell $PROFILE isn't supported yet; add this line yourself:\n{}",
            registration_line(ShellArg::Powershell, &bin_name().unwrap_or_else(|_| "clouduler".into()))
        ),
    })
}

/// Append the registration line to the shell's rc file (fish: write a file under
/// `~/.config/fish/completions/`). Does nothing if already present.
pub fn install(shell: Option<ShellArg>) -> Result<()> {
    warn_if_not_in_group();

    let shell = match shell {
        Some(shell) => shell,
        None => detect_shell()?,
    };
    let bin = bin_name()?;
    let line = registration_line(shell, &bin);
    let path = rc_file(shell)?;

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&line) {
        println!(
            "{} already sources clouduler completions ({})",
            path.display(),
            shell.name()
        );
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    // Fish sources its completion file verbatim, so no `#` comment there.
    if matches!(shell, ShellArg::Fish) {
        writeln!(f, "{line}")
    } else {
        writeln!(f, "\n# added by `clouduler completions install`\n{line}")
    }
    .with_context(|| format!("writing {}", path.display()))?;

    println!(
        "added clouduler {} completions to {}",
        shell.name(),
        path.display()
    );
    println!("start a new shell (or re-source the file) to pick them up");
    Ok(())
}
