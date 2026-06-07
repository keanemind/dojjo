# Sync-server runtime reference

Environment and HTTP surface for `sync-server`. VPS install procedure: [VPS_DEPLOY.md](./VPS_DEPLOY.md).

## Process

Single binary serves:

| Path | Purpose |
|------|---------|
| `/api/…` | REST API, TUS uploads, JJ mirror manifest/objects |
| `/git/<dojo-id>.git/…` | Smart HTTP Git (`git http-backend`) for bare remotes |

`GET /` returns 404 even when healthy. Deploy health check uses `/api/dojo/__dojjo_deploy_probe__`.

Runs as `dojjo` user via systemd (`deploy/systemd/dojjo-sync-server.service`). `WorkingDirectory` and `ReadWritePaths` default to `/var/lib/dojjo`; change the unit if you relocate data.

## Environment variables

Read at startup (`sync-server/src/main.rs`). Also loaded from `sync-server/.env` and cwd `.env` when present (local dev).

### Production (VPS)

Written to `/etc/dojjo/sync-server.env` by `./scripts/deploy-sync-server.sh` from `deploy/deploy.env`:

| Variable | Required | Example | Behavior |
|----------|----------|---------|----------|
| `DATABASE_URL` | yes | `sqlite:///var/lib/dojjo/data.db` | SQLite for dojo metadata. Created if missing. Migrations on start. |
| `DOJJO_DATA_DIR` | yes | `/var/lib/dojjo` | Per-dojo trees: `{id}/mirror/`, `{id}/bare.git`, etc. |
| `DOJJO_LISTEN` | yes* | `100.64.0.1:3000` | `host:port` bind address. *Deploy defaults to `$(tailscale ip -4):3000` when unset in deploy.env. |
| `DOJJO_PUBLIC_URL` | yes** | `http://example.ts.net:3000` | Client-visible origin: `http(s)://host[:port]`, no path, no trailing `/`. Create/join return `{base}/git/<dojo-id>.git`. **Deploy defaults to `http://${DEPLOY_HOST}:3000` when unset. Omit only for local/e2e (`file://` remotes). |
| `RUST_LOG` | no | `info,sync_server=info` | Standard tracing filter (optional in env file). |

### Defaults when unset (local / manual run)

| Variable | Default |
|----------|---------|
| `DATABASE_URL` | `sqlite://<sync-server crate>/data.db` (`sync-server/data.db`) |
| `DOJJO_DATA_DIR` | `<cwd>/dojjo-data` |
| `DOJJO_LISTEN` | `0.0.0.0:3000` |
| `DOJJO_PUBLIC_URL` | unset → `file://` Git remotes under `DOJJO_DATA_DIR` |

### `DOJJO_PUBLIC_URL` validation

Must parse as `http://` or `https://` with host and optional port only. Invalid values panic at startup.

When set, Git remotes use smart HTTP on the tailnet (no TLS or auth in current MVP). Same origin as the API; clients run `git fetch` / `git push` against `/git/…`. No separate SSH Git user or path-rewrite env vars.

## Local / e2e

Run from repo after `./scripts/init-sync-server-db.sh` and `cargo build --bin sync-server`:

```bash
export DATABASE_URL=sqlite:///path/to/data.db
export DOJJO_DATA_DIR=/path/to/dojjo-data
# omit DOJJO_PUBLIC_URL for file:// remotes
cargo run --bin sync-server
```

Or use `sync-server/.env` (see `.env.example`).

## Docker

Root `Dockerfile` — `sync-server` only (Dokku-oriented). Build: `docker build -f Dockerfile .`

| Variable | Image default |
|----------|---------------|
| `DOJJO_DATA_DIR` | `/data` |
| `DOJJO_LISTEN` | `0.0.0.0:3000` |
| `DATABASE_URL` | `sqlite:///data/data.db` |
| `DOJJO_PUBLIC_URL` | unset — set at run time for network Git |

Mount a volume at `DOJJO_DATA_DIR` before relying on persistence. Image build is slow on small VPS; native systemd deploy is preferred for personal servers.
