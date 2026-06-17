# Demos

Two end-to-end demos, deliberately **separate**. The synthetic demo proves the
mechanism with a tiny stateful app and no secrets; the agent demo proves a coding
agent survives a mid-session migration.

## Synthetic demo — a stateful app survives an A→B move

A minimal in-RAM counter app (`examples/ramstate`) on a synthetic guest rootfs. No
agent, no API key. It proves the core property: in-memory state is preserved across a
host migration.

```bash
just prepare        # fresh VM with the synthetic app
just long-process   # terminal 1: client load against the app
just demo-status    # terminal 2 (live): prints on change — location + state
just migrate        # terminal 2: move the VM A→B
```

The counter keeps climbing across the move; the state carried in the snapshot proves
the relocation was transparent.

## Agent demo — a coding agent survives a mid-session migration (O6)

A coding agent ([aider](https://aider.chat/)) on a free model endpoint
([Groq](https://groq.com/) free tier), running inside a Firecracker microVM on the
agent rootfs. You drive turns by hand over HTTP; a migration A↔B lands only between
turns; the agent keeps talking on the new host with state preserved.

**Prerequisites:** `.env` has `AGENT_API_KEY` (a free, spend-limited key); the agent
rootfs built on both hosts (`just agent-rootfs` on each).

```bash
just start-agent          # reset daemons with agent env, overlay + egress + DNAT,
                          # spawn the agent VM on A, wait for its HTTP server

just talk-agent           # terminal 1: type a prompt = one turn; reply printed
just agent-status         # terminal 2 (live): "VM on A/B | turns served: N"
just migrate-when-idle    # terminal 3: wait out any in-flight turn, then move A↔B
```

### How idle-detection works in the demo

The agent's HTTP server is single-threaded, so while it is running a turn it cannot
answer a probe. `migrate-when-idle` does a fast `GET /` (with a short timeout):

- answers ⇒ idle ⇒ migrate now;
- times out ⇒ mid-turn ⇒ wait.

So a migration is only ever attempted in an idle gap, and never orphans a receiver —
the script-level expression of the [race rule](../quiescence/race-rule.md).

### What it demonstrates

- The agent edits files and runs tests per turn against a free model endpoint.
- An A→B migration during an idle gap; turns continue on the new host, **including a
  model call after restore** — the [clock fix-up](../migration/clock-fixup.md) holds.
- A migration fired *mid-turn* **stands down** and keeps the VM alive (the race rule).
- The in-process turn counter carries across the move — state preserved.

### Secret hygiene in this demo

The model API key is delivered to the guest over the `Secrets` vsock message at boot —
never baked into the rootfs, never on the kernel cmdline. Because snapshots are RAM
dumps they contain the key, so use a dedicated, spend-limited, revocable key. See
[Secrets handling](../security/secrets.md).
