# Summary

[Introduction](introduction.md)

# Getting started

- [Quickstart](getting-started/quickstart.md)
- [Development environment](getting-started/environment.md)
- [The `just` target map](getting-started/just-targets.md)

# Architecture

- [System overview](architecture/overview.md)
- [The crates](architecture/crates.md)
- [Data flow](architecture/data-flow.md)

# Fleet rebalancing

- [Overview & a worked example](rebalancing/overview.md)

# The migration pipeline

- [Overview & the state machine](migration/overview.md)
- [Source side](migration/source.md)
- [Target side & UFFD lazy restore](migration/target-uffd.md)
- [Snapshot transfer](migration/transfer.md)
- [Post-restore clock fix-up](migration/clock-fixup.md)

# Quiescence & the race rule

- [Layered quiescence](quiescence/layers.md)
- [The drain protocol & race rule](quiescence/race-rule.md)

# Guest protocol

- [Protocol & state machine](protocol.md)

# Operations

- [CLI & configuration](operations/cli.md)
- [Networking](operations/networking.md)
- [Observability](operations/observability.md)
- [Demos](operations/demos.md)

# Security

- [Secrets handling](security/secrets.md)
- [CPU/TSC compatibility (ADR-004)](security/cpu-tsc.md)

# Reference

- [Glossary](reference/glossary.md)
- [Limitations](reference/limitations.md)
