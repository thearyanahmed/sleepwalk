# CLI & configuration

The `sleepwalk` binary is the front door. It is the **only** crate published to
crates.io.

> **Status:** parsing and config loading are real and tested; the handlers that need a
> running host runtime are stubbed (`not_wired`) and fail with a clear message until
> that runtime is wired. Today the system is driven through `just` targets and the
> helper binaries — see [The `just` target map](../getting-started/just-targets.md).

## Command surface

```text
sleepwalk [--config sleepwalk.toml] <command>

host run                       Start the per-host daemon (hostd).
vm create [--profile NAME]     Create a VM (default profile: synthetic).
vm list                        List the VMs on this host.
vm status <vm>                 Show one VM's status.
migrate <vm> --to <host>       Migrate a VM to another host (quiescence-gated).
rebalance [--watch]            Run the autonomous rebalancer.
quiesce <vm>                   Inspect a VM's live quiescence predicate.
```

`--config` is global and defaults to `sleepwalk.toml`; if the file is absent the
documented defaults apply.

## Configuration

TOML, every key optional, the shown values are the defaults — so an empty file behaves
identically. **Unknown keys are rejected**, so a typo fails loudly instead of being
silently ignored.

```toml
# Where per-VM state directories live (one subdir per VM; the jailer chroots here).
state_dir = "/var/lib/sleepwalk"

[quiescence]
# A vCPU utilisation sample below this percent counts as "quiet".
cpu_pct = 5.0
# Consecutive quiet samples required before the infra layer is considered quiet.
samples = 5
# Milliseconds between quiescence samples.
sample_interval_ms = 200

[migration]
# How long to wait for an in-flight turn before aborting a drain (the turn wins).
drain_deadline_ms = 5000
```

| Key | Default | Meaning |
|-----|---------|---------|
| `state_dir` | `/var/lib/sleepwalk` | Per-VM state directories; the jailer chroots here. |
| `quiescence.cpu_pct` | `5.0` | vCPU % below which a sample counts as quiet (the [infra layer](../quiescence/layers.md)). |
| `quiescence.samples` | `5` | Consecutive quiet samples required before the infra layer is quiet. |
| `quiescence.sample_interval_ms` | `200` | Milliseconds between quiescence samples. |
| `migration.drain_deadline_ms` | `5000` | How long to wait for an in-flight turn before aborting a drain — the turn wins. |

The quiescence and drain values are not arbitrary: they are themselves a **measured
output** of the benchmark phase, and they directly parameterise the
[layered quiescence detector](../quiescence/layers.md) and the
[race rule](../quiescence/race-rule.md).
