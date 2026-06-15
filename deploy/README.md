# deploy — observability stack

Prometheus + Grafana that scrape the `hostd` daemons (and `node_exporter` on each
host) and chart the fleet: which servers are up, machine resources, where each VM
runs, the load's request rate, and migrations. Local, throwaway, anonymous-admin,
forced light theme — not a production deployment.

## Run

```
just observe          # docker compose up; Grafana at http://localhost:3000
just observe-down     # tear it down
```

Grafana opens on the **sleepwalk — fleet, VMs & load** dashboard (anonymous login,
light theme), laid out in three rows:

- **Fleet** — servers by IP (up/down), CPU busy %, memory used % per host.
- **VMs** — `vm_id → host / ip` placement table and VMs-per-host. On a migration
  the same `vm_id`'s host/ip flips and the per-host counts cross over.
- **Load & migrations** — request rate (ok turns/s) and dropped turns/s per VM,
  plus migrations/failures/bytes and the freeze-window p50/p99. Migration events
  are drawn as red annotations across every time panel, so you can see the RPS
  line hold flat through the freeze.

## Point it at your daemons

Each `hostd` must serve on a reachable address:

```
hostd daemon 0.0.0.0:8080
```

List the daemons in `prometheus/targets.json` (file discovery, hot-reloaded — no
restart). It is **gitignored** so host addresses stay out of version control;
`just observe` seeds it from `targets.json.example` on first run:

```json
[
  { "labels": { "job": "hostd" }, "targets": ["10.0.0.1:8080", "10.0.0.2:8080"] }
]
```

## Machine resources (node_exporter)

Install and start `node_exporter` on each host once:

```
scripts/host.sh a node-exporter
scripts/host.sh b node-exporter
```

Then list them in `prometheus/node-targets.json` (also gitignored, seeded from
`node-targets.json.example`). Give each a `host` label that **matches that host's
hostd id**, so the dashboard joins machine resources to VMs by host:

```json
[
  { "labels": { "job": "node", "host": "a" }, "targets": ["10.0.0.1:9100"] },
  { "labels": { "job": "node", "host": "b" }, "targets": ["10.0.0.2:9100"] }
]
```

## Layout

| Path | Purpose |
|------|---------|
| `docker-compose.yml` | Prometheus + Grafana services (light theme forced) |
| `prometheus/prometheus.yml` | scrape config — `hostd` + `node` jobs |
| `prometheus/targets.json` | the hostd endpoints to scrape |
| `prometheus/node-targets.json` | the node_exporter endpoints to scrape |
| `grafana/provisioning/` | datasource + dashboard provider (auto-loaded) |
| `grafana/dashboards/sleepwalk.json` | the fleet / VMs / load dashboard |
