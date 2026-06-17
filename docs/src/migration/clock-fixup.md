# Post-restore clock fix-up

A migrated guest wakes up believing no time has passed. Its wall clock **froze at
snapshot time** and resumes from that frozen value on the target — but the real world
moved forward by the whole migration (drain wait + snapshot + transfer + restore). Left
uncorrected, that skew is not cosmetic:

- **TLS** — a certificate the guest sees as still-valid (or not-yet-valid) against its
  stale clock; handshakes fail or wrongly succeed.
- **Token expiry** — an API token the guest thinks is fresh has actually expired, or
  vice versa. The agent profile's model calls hit this directly.
- **Comparability** — a timestamp recorded before the move and one recorded after sit
  on two different timelines; subtracting them is meaningless until both are corrected.

## The fix: host clock is the truth

The host process never froze, so its clock is authoritative. The correction is the
signed offset between what the guest reads at resume and what the host reads for that
same instant:

```
offset = authoritative_host_time − guest_frozen_time
```

Apply that offset and a frozen guest reading maps back onto the true timeline.

```
snapshot froze the guest clock at   1_000
migration took (real wall time)      +500
host's authoritative clock at resume 1_500   ← never froze
                                     ─────
offset = 1_500 − 1_000             = +500

a turn the guest later stamps at     1_200  (still behind)
corrected: 1_200 + 500             = 1_700  ← back on the true timeline
```

The arithmetic is pure and lives in `guestd`'s `ClockFixup` (`crates/guestd/src/clock.rs`):

- `ClockFixup::between(guest_at_resume, authoritative)` computes the offset.
- `correct(observed)` maps any later guest reading forward.
- `none()` is the identity (no migration, no skew).
- A backward correction that would push a timestamp below the epoch **saturates at
  zero**, never wraps into a nonsense instant.

## Where guestd fits in the sequence

The fix-up is anchored to the **`Resumed`** message — the first thing `guestd` sends on
the target after a restore (see the [protocol](../protocol.md) and the
[rebalancing walkthrough](../rebalancing/overview.md#one-move-component-by-component)):

1. Target restores the snapshot and resumes the VM.
2. `guestd` sends **`Resumed`** and reads its own (still-frozen) clock.
3. The host supplies the authoritative wall-clock for that instant.
4. `ClockFixup::between(...)` yields the offset; applying it to the live system clock
   (`clock_settime`) jumps the guest onto true time.

So `guestd` does not *measure* the truth on its own — the host provides it. `guestd`
announces the resume, reads the frozen value, and applies the jump locally. After that,
the agent's next model call carries a correct clock.

## Status

The `ClockFixup` arithmetic is implemented and unit-tested; it is what the measurement
harness uses to line up turn latencies recorded on either side of a move. Applying the
correction to the live guest system clock is the guest-OS side and is **not yet wired
on every restore path** — it has not bitten at the small freeze windows measured so
far, but a long freeze could cause TLS/token skew. Tracked in
[Limitations](../reference/limitations.md). Pre-v0.1.0.

> RNG state is also duplicated across a restore. Irrelevant for a single migration;
> relevant only for snapshot *forking*, which sleepwalk does not do. Noted for
> completeness.
