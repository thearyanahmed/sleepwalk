# Observability

A live observability stack, fully provisioned from the repo, plus the rule for which
numbers are authoritative.

## Bring it up

```bash
just observe       # Prometheus + Grafana via docker compose; Grafana at :3000
just observe-down  # tear it down
```

`deploy/docker-compose.yml` brings up Prometheus + Grafana with dashboards and the
datasource provisioned from committed JSON/YAML — zero clicking. Point it at your
`hostd` daemons by editing `deploy/prometheus/targets.json` (gitignored; an
`.example` is copied on first run).

## What each daemon exports

Every Rust daemon exposes `/metrics` (and `/healthz`).

| Daemon | Key metrics |
|--------|-------------|
| **hostd** | per-VM vCPU %, quiescence state (a gauge per layer), UFFD faults served, pages prefetched, snapshot bytes streamed, freeze-window histogram, FC process up, `sleepwalk_host_info{host,class}` (the [compatibility class](../security/cpu-tsc.md)). |
| **rebalancer** | FSM state per migration (gauge), migrations started/completed/aborted, drain-duration histogram, host memory pressure, VMs per host. |
| **harness** | request rate, in-flight, latency histogram, error/timeout counters. |

## Migration events as Grafana annotations

The rebalancer posts FSM transitions (`Intent`, `Snapshotting`, `CutOver`, `Abort`) to
Grafana's annotation API, so the latency graph shows **vertical markers exactly where
migrations happened**. The demo visual is the p99 latency line staying flat *through*
those markers — the migration is invisible to the workload.

Committed dashboards: **Fleet** (placement, pressure, VM states), **Migration**
(per-migration timeline: freeze / drain / transfer), **Latency** (loadgen p50/p99 with
the annotations).

## The scope rule — which numbers win

> Prometheus/Grafana is the **live view and demo layer**, not the measurement
> instrument.

Scrape intervals (1–15 s) and histogram bucketing are too coarse for sub-second
freeze-window and idle-gap claims. **Benchmark numbers always come from the
[harness](../architecture/crates.md#harness)'s raw per-request logs / HdrHistograms and
the JSON transcripts in `results/`.** The two views must agree directionally; where
they differ, the raw logs win. This avoids quoting a dashboard average as if it were a
measured p99.
