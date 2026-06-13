# `sleepwalk` (cli)

The `sleepwalk` binary — the front door, and the **only** crate published to
crates.io. Wraps the internal crates behind a single command.

## Commands

```
sleepwalk host run                 # start the per-host daemon
sleepwalk vm create [--profile P]  # create a VM
sleepwalk vm list                  # list VMs on this host
sleepwalk vm status <vm>           # one VM's status
sleepwalk migrate <vm> --to <host> # quiescence-gated relocation
sleepwalk rebalance [--watch]      # autonomous drain-and-relocate loop
sleepwalk quiesce <vm>             # inspect the live quiescence predicate
```

`--config <path>` (default `sleepwalk.toml`) is global. Configuration is
optional — a missing file uses the documented defaults; see
[`sleepwalk.example.toml`](../../sleepwalk.example.toml). Unknown keys are
rejected so typos fail loudly.

## Status

This slice is the command surface and config loading. Handlers that need the host
runtime (spawning Firecracker, talking to a running hostd) print a clear "not
wired yet" error rather than pretending to act; they are filled in as that
runtime lands.

## Testing it in isolation

```
cargo test -p sleepwalk    # argument parsing + config loading
```
