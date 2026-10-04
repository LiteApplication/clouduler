//! The iptables plumbing shared by the egress kill-switch, the container
//! input filter and the wireguard ingress rules.
//!
//! Every rule set here is described as data first ([`IptablesPlan`]) and only
//! then applied, so the exact rules are unit tested without root. Applying has
//! two rules of its own:
//!
//! * A chain's contents are replaced with one `iptables-restore --noflush`
//!   transaction, never flush-then-append. A flush-then-append leaves a window
//!   where a kill-switch chain is empty (traffic leaks) or a filter chain has
//!   lost its accept rules (traffic is dropped), once per reconcile tick.
//! * A jump into a chain is only ever added after the chain is filled, and is
//!   existence checked (`-C`) each tick rather than assumed, so it survives a
//!   docker restart that rewrites the parent chain.
//!
//! Every invocation uses `-w`: the daemon, docker and the operator all touch
//! the xtables lock, and without the wait the loser fails with exit 4.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::{NetError, Result};

/// One chain's complete desired contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSpec {
    /// `filter` or `nat`.
    pub table: &'static str,
    pub name: String,
    /// Each rule as the text after `-A <name> `, in evaluation order.
    pub rules: Vec<String>,
}

impl ChainSpec {
    pub fn new(table: &'static str, name: impl Into<String>) -> Self {
        Self {
            table,
            name: name.into(),
            rules: Vec::new(),
        }
    }

    /// Append a rule given as separate arguments.
    pub fn push(&mut self, rule: &[&str]) {
        self.rules.push(rule.join(" "));
    }
}

/// Where a chain gets called from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jump {
    pub table: &'static str,
    /// Candidate parent chains in order of preference. The first that exists
    /// receives the jump; it is removed from any later candidate it is still
    /// found in. `["DOCKER-USER", "FORWARD"]` therefore migrates a jump out of
    /// `FORWARD` once docker has made `DOCKER-USER`.
    pub parents: Vec<String>,
    pub chain: String,
    /// Insert at position 1 instead of appending. Needed to run ahead of
    /// docker's own accept rules.
    pub first: bool,
}

impl Jump {
    pub fn new(table: &'static str, parents: &[&str], chain: &str, first: bool) -> Self {
        Self {
            table,
            parents: parents.iter().map(|p| p.to_string()).collect(),
            chain: chain.to_string(),
            first,
        }
    }
}

/// Chains to fill and the jumps that reach them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IptablesPlan {
    pub chains: Vec<ChainSpec>,
    pub jumps: Vec<Jump>,
}

impl IptablesPlan {
    pub fn chain(&self, name: &str) -> Option<&ChainSpec> {
        self.chains.iter().find(|c| c.name == name)
    }
}

/// The `iptables-restore --noflush` input that sets every chain in `chains`.
///
/// A chain is declared (`:NAME - [0:0]`, which creates it if absent) and then
/// explicitly flushed and refilled inside the same transaction, so the kernel
/// swaps old contents for new in one step. `--noflush` is what keeps every
/// chain this input does not mention, docker's included, untouched.
pub fn restore_payload(chains: &[ChainSpec]) -> String {
    let mut tables: Vec<&'static str> = Vec::new();
    for chain in chains {
        if !tables.contains(&chain.table) {
            tables.push(chain.table);
        }
    }
    let mut out = String::new();
    for table in tables {
        out.push_str(&format!("*{table}\n"));
        let in_table: Vec<&ChainSpec> = chains.iter().filter(|c| c.table == table).collect();
        for chain in &in_table {
            out.push_str(&format!(":{} - [0:0]\n", chain.name));
        }
        for chain in &in_table {
            out.push_str(&format!("-F {}\n", chain.name));
            for rule in &chain.rules {
                out.push_str(&format!("-A {} {rule}\n", chain.name));
            }
        }
        out.push_str("COMMIT\n");
    }
    out
}

/// Does `listing` (the `-S <parent>` output) already hold a rule in `parent`
/// jumping to `chain`, and if so, is it where it should be?
///
/// Returns `None` when there is no such jump. For a `first` jump the position
/// is acceptable when nothing but other clouduler jumps (`-j CLDR-...`) sits
/// ahead of it. Strictly "position 1" would make two clouduler chains that both
/// want the top of the same parent evict each other on every tick.
fn jump_state(listing: &str, parent: &str, chain: &str, first: bool) -> Option<bool> {
    let prefix = format!("-A {parent} ");
    let mut only_ours_before = true;
    for line in listing.lines().filter(|l| l.starts_with(&prefix)) {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let target = tokens
            .windows(2)
            .find(|w| w[0] == "-j" || w[0] == "-g")
            .map(|w| w[1]);
        if target == Some(chain) {
            return Some(!first || only_ours_before);
        }
        if !target.is_some_and(|t| t.starts_with("CLDR-")) {
            only_ours_before = false;
        }
    }
    None
}

/// The commands that bring one jump in line, given what exists right now.
///
/// Pure so the decision (which parent, what to delete, where to insert) is
/// tested without a firewall. `listing` returns a chain's `-S` output, or
/// `None` if the chain does not exist.
pub fn jump_commands(jump: &Jump, listing: &dyn Fn(&str) -> Option<String>) -> Vec<Vec<String>> {
    let listings: Vec<(&String, Option<String>)> =
        jump.parents.iter().map(|p| (p, listing(p))).collect();
    let target = listings
        .iter()
        .find(|(_, l)| l.is_some())
        .map(|(p, _)| *p)
        .or_else(|| jump.parents.last());
    let Some(target) = target else {
        return Vec::new();
    };

    let mut commands = Vec::new();
    let mut present_ok = false;
    for (parent, listing) in &listings {
        let Some(listing) = listing else { continue };
        let state = jump_state(listing, parent, &jump.chain, jump.first);
        if *parent == target {
            match state {
                Some(true) => present_ok = true,
                // Present but behind someone else's rules: take it out and
                // put it back at the top below.
                Some(false) => {
                    commands.push(table_args(jump.table, &["-D", parent, "-j", &jump.chain]))
                }
                None => {}
            }
        } else if state.is_some() {
            commands.push(table_args(jump.table, &["-D", parent, "-j", &jump.chain]));
        }
    }
    if !present_ok {
        let args: Vec<&str> = if jump.first {
            vec!["-I", target, "1", "-j", &jump.chain]
        } else {
            vec!["-A", target, "-j", &jump.chain]
        };
        commands.push(table_args(jump.table, &args));
    }
    commands
}

fn table_args(table: &str, args: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    if table != "filter" {
        out.push("-t".to_string());
        out.push(table.to_string());
    }
    out.extend(args.iter().map(|a| a.to_string()));
    out
}

/// Apply a plan: fill every chain atomically, then make sure each jump is in
/// place. Safe to call every reconcile tick.
pub fn apply_plan(plan: &IptablesPlan) -> Result<()> {
    if !plan.chains.is_empty() {
        iptables_restore(&restore_payload(&plan.chains))?;
    }
    for jump in &plan.jumps {
        apply_jump(jump)?;
    }
    Ok(())
}

fn apply_jump(jump: &Jump) -> Result<()> {
    let table = jump.table;
    let listing = |parent: &str| iptables_output(&table_args(table, &["-S", parent])).ok();
    for command in jump_commands(jump, &listing) {
        run_iptables(&command)?;
    }
    Ok(())
}

/// Run `iptables -w <args>`.
pub fn run_iptables(args: &[String]) -> Result<()> {
    iptables_output(args).map(|_| ())
}

/// Run `iptables -w <args>` and return its stdout.
pub fn iptables_output(args: &[String]) -> Result<String> {
    let output = Command::new("iptables")
        .arg("-w")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: format!("iptables -w {}", args.join(" ")),
            source: e,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(NetError::Io {
            context: format!(
                "iptables -w {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ),
            source: std::io::Error::other("iptables exited non-zero"),
        })
    }
}

fn iptables_restore(payload: &str) -> Result<()> {
    let mut child = Command::new("iptables-restore")
        .args(["-w", "--noflush"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| NetError::Io {
            context: "iptables-restore".to_string(),
            source: e,
        })?;
    // Dropping the handle closes the pipe, which is what tells restore the
    // input is complete.
    let write = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(payload.as_bytes()));
    let output = child.wait_with_output().map_err(|e| NetError::Io {
        context: "iptables-restore".to_string(),
        source: e,
    })?;
    if let Some(Err(e)) = write {
        return Err(NetError::Io {
            context: "writing iptables-restore input".to_string(),
            source: e,
        });
    }
    if output.status.success() {
        Ok(())
    } else {
        Err(NetError::Io {
            context: format!(
                "iptables-restore: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            source: std::io::Error::other("iptables-restore exited non-zero"),
        })
    }
}

/// Names of the user chains in `-S` output (`-N NAME` lines).
pub fn parse_chain_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.strip_prefix("-N "))
        .map(|name| name.trim().to_string())
        .collect()
}

/// The `-A` rules in `-S` output that jump to `chain`, rewritten as the `-D`
/// that removes each.
pub fn parse_jump_deletions(listing: &str, chain: &str) -> Vec<Vec<String>> {
    listing
        .lines()
        .filter(|line| line.starts_with("-A "))
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let jumps_here = tokens
                .windows(2)
                .any(|w| (w[0] == "-j" || w[0] == "-g") && w[1] == chain);
            jumps_here.then(|| {
                let mut args: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
                args[0] = "-D".to_string();
                args
            })
        })
        .collect()
}

/// User chains in `table`, from the live ruleset.
pub fn list_chains(table: &str) -> Result<Vec<String>> {
    Ok(parse_chain_names(&iptables_output(&table_args(
        table,
        &["-S"],
    ))?))
}

/// Remove a chain along with every jump to it: unhook, flush, delete. A chain
/// still referenced cannot be deleted, so the jumps go first.
pub fn remove_chain(table: &str, chain: &str) -> Result<()> {
    let listing = iptables_output(&table_args(table, &["-S"]))?;
    for deletion in parse_jump_deletions(&listing, chain) {
        let mut args = Vec::new();
        if table != "filter" {
            args.push("-t".to_string());
            args.push(table.to_string());
        }
        args.extend(deletion);
        run_iptables(&args)?;
    }
    run_iptables(&table_args(table, &["-F", chain]))?;
    run_iptables(&table_args(table, &["-X", chain]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(table: &'static str, name: &str, rules: &[&str]) -> ChainSpec {
        let mut c = ChainSpec::new(table, name);
        for r in rules {
            c.rules.push(r.to_string());
        }
        c
    }

    #[test]
    fn restore_input_declares_flushes_and_fills_each_chain_in_one_transaction() {
        let payload = restore_payload(&[
            chain("nat", "A", &["-s 1.2.3.0/24 -j MASQUERADE"]),
            chain("filter", "B", &["-j DROP"]),
            chain("filter", "C", &[]),
        ]);
        assert_eq!(
            payload,
            "*nat\n:A - [0:0]\n-F A\n-A A -s 1.2.3.0/24 -j MASQUERADE\nCOMMIT\n\
             *filter\n:B - [0:0]\n:C - [0:0]\n-F B\n-A B -j DROP\n-F C\nCOMMIT\n"
        );
    }

    const DOCKER_USER: &str = "-N DOCKER-USER\n-A DOCKER-USER -j RETURN\n";

    fn listings<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |parent| {
            pairs
                .iter()
                .find(|(p, _)| *p == parent)
                .map(|(_, l)| l.to_string())
        }
    }

    #[test]
    fn a_jump_goes_into_docker_user_when_it_exists_and_leaves_forward() {
        let jump = Jump::new("filter", &["DOCKER-USER", "FORWARD"], "X", true);
        let cmds = jump_commands(
            &jump,
            &listings(&[
                ("DOCKER-USER", DOCKER_USER),
                ("FORWARD", "-A FORWARD -j X\n"),
            ]),
        );
        assert_eq!(
            cmds,
            vec![
                argv(&["-D", "FORWARD", "-j", "X"]),
                argv(&["-I", "DOCKER-USER", "1", "-j", "X"]),
            ]
        );
    }

    #[test]
    fn a_jump_falls_back_to_forward_without_docker_user() {
        let jump = Jump::new("filter", &["DOCKER-USER", "FORWARD"], "X", true);
        let cmds = jump_commands(&jump, &listings(&[("FORWARD", "-A FORWARD -j DOCKER\n")]));
        assert_eq!(cmds, vec![argv(&["-I", "FORWARD", "1", "-j", "X"])]);
    }

    #[test]
    fn a_jump_already_at_the_top_changes_nothing() {
        let jump = Jump::new("filter", &["DOCKER-USER", "FORWARD"], "X", true);
        let l = "-A DOCKER-USER -j X\n-A DOCKER-USER -j RETURN\n";
        assert!(jump_commands(&jump, &listings(&[("DOCKER-USER", l), ("FORWARD", "")])).is_empty());
    }

    #[test]
    fn other_clouduler_jumps_ahead_are_not_evicted() {
        let jump = Jump::new("filter", &["INPUT"], "CLDR-B", true);
        let l = "-A INPUT -j CLDR-A\n-A INPUT -j CLDR-B\n-A INPUT -j ACCEPT\n";
        assert!(jump_commands(&jump, &listings(&[("INPUT", l)])).is_empty());
    }

    #[test]
    fn a_jump_left_at_the_end_by_an_older_version_is_moved_to_the_top() {
        let jump = Jump::new("filter", &["DOCKER-USER", "FORWARD"], "X", true);
        let l = "-A FORWARD -j DOCKER-USER\n-A FORWARD -j DOCKER\n-A FORWARD -j X\n";
        assert_eq!(
            jump_commands(&jump, &listings(&[("FORWARD", l)])),
            vec![
                argv(&["-D", "FORWARD", "-j", "X"]),
                argv(&["-I", "FORWARD", "1", "-j", "X"]),
            ]
        );
    }

    #[test]
    fn a_nat_jump_names_its_table_and_only_needs_to_exist() {
        let jump = Jump::new("nat", &["POSTROUTING"], "N", false);
        assert_eq!(
            jump_commands(
                &jump,
                &listings(&[("POSTROUTING", "-A POSTROUTING -j DOCKER\n")])
            ),
            vec![argv(&["-t", "nat", "-A", "POSTROUTING", "-j", "N"])]
        );
        assert!(jump_commands(
            &jump,
            &listings(&[(
                "POSTROUTING",
                "-A POSTROUTING -j DOCKER\n-A POSTROUTING -j N\n"
            )])
        )
        .is_empty());
    }

    #[test]
    fn listings_are_parsed_for_chains_and_the_jumps_into_them() {
        let listing = "-P INPUT ACCEPT\n-N CLDR-FWD-x\n-N DOCKER-USER\n\
                       -A FORWARD -j DOCKER-USER\n-A DOCKER-USER -j CLDR-FWD-x\n\
                       -A CLDR-FWD-x -i clex-x -j DROP\n";
        assert_eq!(
            parse_chain_names(listing),
            vec!["CLDR-FWD-x", "DOCKER-USER"]
        );
        assert_eq!(
            parse_jump_deletions(listing, "CLDR-FWD-x"),
            vec![argv(&["-D", "DOCKER-USER", "-j", "CLDR-FWD-x"])]
        );
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }
}
