# deploy — observability stack

Prometheus + Grafana that scrape the `hostd` daemons and chart migrations and the
freeze window. Local, throwaway, anonymous-admin — not a production deployment.

## Run

```
just observe          # docker compose up; Grafana at http://localhost:3000
just observe-down     # tear it down
```

Grafana opens straight onto the **sleepwalk — migrations** dashboard (anonymous
login is on): migrations completed, failures, bytes moved, migration rate, and
the freeze-window p50/p99.

## Point it at your daemons

Each daemon must serve on a reachable address:

```
hostd daemon 0.0.0.0:8080
```

List the daemons in `prometheus/targets.json` (file discovery, hot-reloaded — no
restart needed). It is **gitignored** so your host addresses stay out of version
control; `just observe` seeds it from `targets.json.example` on first run:

```json
[
  { "labels": { "job": "hostd" }, "targets": ["10.0.0.1:8080", "10.0.0.2:8080"] }
]
```

The default target is `host.docker.internal:8080` — a `hostd` running on the same
machine as the stack. For remote hosts (droplets), use their addresses and make
sure `:8080` is reachable from where the stack runs.

## Layout

| Path | Purpose |
|------|---------|
| `docker-compose.yml` | Prometheus + Grafana services |
| `prometheus/prometheus.yml` | scrape config (file-based target discovery) |
| `prometheus/targets.json` | the hostd endpoints to scrape |
| `grafana/provisioning/` | datasource + dashboard provider (auto-loaded) |
| `grafana/dashboards/sleepwalk.json` | the migrations dashboard |
