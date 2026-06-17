# Secrets handling

A workload may need a secret at startup — a model API key, a token. sleepwalk delivers
it without ever writing it where it can leak, and is honest about the one place it
unavoidably ends up: the snapshot.

## Delivery: the `Secrets` vsock message, at boot only

`hostd` reads the secret from its own environment (`.env`, untracked) and hands it to
the guest over the `Secrets` vsock message at boot. `guestd` sets it in the
environment and execs the workload.

```
hostd env (.env)  ──Secrets{env}──▶  guestd  ──exec with env──▶  workload
```

It is **never**:

- baked into the rootfs image (world-readable from the host), or
- placed on the kernel cmdline (visible in `/proc/cmdline` and host `ps`).

In [wrap mode](../protocol.md), a workload that needs the secret *at exec* sets
`/etc/sleepwalk/wrap-await-secrets` in the rootfs; `guestd` then defers spawning the
child until the first handshake delivers `Secrets`, and spawns it with them in its
environment. The same value persists in the running process across a migration.

## The unavoidable fact: snapshots contain the secret

> A snapshot is a RAM dump. If the secret is in guest memory, it is in the snapshot.

This is treated as a design fact, not a bug, and mitigated by mechanism:

- Snapshot directories are `0700`.
- Snapshots are transferred only over sleepwalk's own channel — never uploaded to
  third-party storage.
- Snapshots are deleted at `Cleanup`.
- **Use a dedicated, spend-limited, revocable key.** If a snapshot leaks, the blast
  radius is one cheap key you can rotate.

## The production answer: a credentials broker (ADR-005)

The way to keep the secret out of snapshots entirely is to keep it out of guest memory
entirely: a **credentials broker** holds the key, and the guest fetches a short-lived
credential when it needs one. Then snapshot / fork / migrate never replicates the
secret. This is the documented production recommendation; the in-guest `Secrets`
handoff is the PoC's pragmatic choice, chosen precisely so that even a *free* key still
exercises the secret-handoff and snapshot-hygiene paths.

## Operator checklist

- [ ] The key in `.env` is free or spend-limited, and revocable.
- [ ] `.env` is gitignored (it is) and never committed.
- [ ] Snapshot directories stay `0700` and are cleaned up after migration.
- [ ] Never `bash -x` a script that sources `.env` — it echoes the key into logs.
