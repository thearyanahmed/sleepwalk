//! The `sleepwalk` binary — the front door to the whole system.
//!
//! This is the command surface and config loading. The handlers that need the
//! host runtime (spawning Firecracker, talking to a running hostd) are stubs
//! until that runtime is wired; the parsing and configuration are real.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

mod config;
use config::Config;

/// Zero-perceived-downtime Firecracker microVM rebalancing.
#[derive(Parser, Debug)]
#[command(name = "sleepwalk", version, about)]
struct Cli {
    /// Path to the TOML config (optional; defaults are used if absent).
    #[arg(long, default_value = "sleepwalk.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the per-host daemon.
    Host {
        #[command(subcommand)]
        action: HostAction,
    },
    /// Create and inspect VMs on this host.
    Vm {
        #[command(subcommand)]
        action: VmAction,
    },
    /// Migrate a VM to another host (quiescence-gated).
    Migrate {
        /// The VM to move.
        vm: String,
        /// The target host.
        #[arg(long)]
        to: String,
    },
    /// Run the autonomous rebalancer.
    Rebalance {
        /// Keep running, draining VMs off pressured hosts as gaps appear.
        #[arg(long)]
        watch: bool,
    },
    /// Inspect a VM's live quiescence predicate.
    Quiesce {
        /// The VM to inspect.
        vm: String,
    },
}

#[derive(Subcommand, Debug)]
enum HostAction {
    /// Start hostd.
    Run,
}

#[derive(Subcommand, Debug)]
enum VmAction {
    /// Create a VM.
    Create {
        /// The rootfs profile to boot.
        #[arg(long, default_value = "synthetic")]
        profile: String,
    },
    /// List the VMs on this host.
    List,
    /// Show one VM's status.
    Status {
        /// The VM to inspect.
        vm: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    dispatch(cli.command, &config)
}

fn dispatch(command: Command, _config: &Config) -> Result<()> {
    match command {
        Command::Host { action } => match action {
            HostAction::Run => not_wired("host run"),
        },
        Command::Vm { action } => match action {
            VmAction::Create { .. } => not_wired("vm create"),
            VmAction::List => not_wired("vm list"),
            VmAction::Status { .. } => not_wired("vm status"),
        },
        Command::Migrate { .. } => not_wired("migrate"),
        Command::Rebalance { .. } => not_wired("rebalance"),
        Command::Quiesce { .. } => not_wired("quiesce"),
    }
}

/// The handlers that need a running host runtime aren't wired yet. Fail clearly
/// rather than pretending to act.
fn not_wired(what: &str) -> Result<()> {
    bail!("`{what}` needs the host runtime, which is not wired yet — see ROADMAP")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn migrate_parses_vm_and_target() {
        let cli = parse(&["sleepwalk", "migrate", "vm-1", "--to", "host-b"]).expect("parse");
        match cli.command {
            Command::Migrate { vm, to } => {
                assert_eq!(vm, "vm-1");
                assert_eq!(to, "host-b");
            }
            other => panic!("expected Migrate, got {other:?}"),
        }
    }

    #[test]
    fn vm_create_defaults_to_synthetic_profile() {
        let cli = parse(&["sleepwalk", "vm", "create"]).expect("parse");
        match cli.command {
            Command::Vm {
                action: VmAction::Create { profile },
            } => assert_eq!(profile, "synthetic"),
            other => panic!("expected Vm Create, got {other:?}"),
        }
    }

    #[test]
    fn global_config_flag_is_accepted_after_the_subcommand() {
        let cli = parse(&["sleepwalk", "host", "run", "--config", "/etc/sw.toml"]).expect("parse");
        assert_eq!(cli.config, PathBuf::from("/etc/sw.toml"));
        assert!(matches!(
            cli.command,
            Command::Host {
                action: HostAction::Run
            }
        ));
    }

    #[test]
    fn migrate_requires_the_to_flag() {
        assert!(parse(&["sleepwalk", "migrate", "vm-1"]).is_err());
    }

    #[test]
    fn no_subcommand_is_an_error() {
        assert!(parse(&["sleepwalk"]).is_err());
    }
}
