# VPS deploy

Native `sync-server` install on a Linux VPS over Tailscale: systemd unit, `dojjo` system user, SQLite and dojo data under `/var/lib/dojjo`. Entry point: `./scripts/deploy-sync-server.sh`.

Two machines involved:

| Role | What it is |
|------|------------|
| **Deploy host** | Where you clone the repo and run `./scripts/deploy-sync-server.sh`. Needs Rust toolchain access and SSH to the VPS. Any OS (Linux, macOS, …). |
| **VPS** | `DEPLOY_HOST` — Linux install target. Receives the binary and env file over SSH; runs `sync-server` under systemd. |

Compile and install are separate steps. Default strategies compile on the **deploy host** and copy one Linux binary to the VPS. `BUILD_STRATEGY=remote` compiles on the VPS instead.

Server env vars: [SYNC_SERVER_DEPLOY.md](./SYNC_SERVER_DEPLOY.md).

## Prerequisites

### Deploy host

- SSH to `DEPLOY_USER@DEPLOY_HOST` over the tailnet (BatchMode for non-interactive steps; `ssh -t` when sudo needs a password).
- `sqlx-cli` — `./scripts/init-sync-server-db.sh` runs before every compile so `sqlx::query!` checks pass. Install: `cargo install sqlx-cli --no-default-features --features rustls,sqlite` (or `cargo sqlx` via `mise`).
- Build tools per `BUILD_STRATEGY` (below).

The script’s **`auto`** strategy is tuned for one common case: **Apple Silicon Mac → x86_64 Linux VPS** uses **`cargo-zigbuild`** on the deploy host (no Docker). All other `auto` combinations fall through to **`cross`** (Docker on the deploy host).

| Strategy | Build runs on | Typical use |
|----------|---------------|-------------|
| `auto` | Deploy host | Default. Apple Silicon + x86_64 VPS → zigbuild; otherwise → cross |
| `zigbuild` | Deploy host | Explicit zig cross-compile (`cargo-zigbuild --target …`) |
| `cross` | Deploy host (Docker) | Explicit cross-rs; also what `auto` picks except Apple Silicon → x86_64 |
| `remote` | VPS | rsync sources + native `cargo build` on the VPS |

**zigbuild** (Apple Silicon Mac deploy host → x86_64 Linux VPS; also `BUILD_STRATEGY=zigbuild`):

```bash
cargo install --locked cargo-zigbuild
rustup target add x86_64-unknown-linux-gnu
# zig on PATH (mise, brew, package manager, …)
```

**cross** (`BUILD_STRATEGY=cross`, or `auto` when zigbuild is not selected):

```bash
cargo install cross --git https://github.com/cross-rs/cross
# Docker daemon running on the deploy host
```

Do not use crates.io `cross` 0.2.5 — it breaks on Rust 1.92+.

**remote** (`BUILD_STRATEGY=remote`): no cross toolchain on the deploy host; rsyncs the repo to the VPS and runs `cargo build` there. One-time VPS setup via `deploy/remote/build-deps.sh` (Rust via rustup, build packages). Sensible when the deploy host is Linux and matches the VPS architecture, or when you prefer not to use Docker locally.

**Not wired in the script:** same-arch `cargo build --release -p sync-server` on the deploy host, then `--install-only`. Works if you produce a Linux ELF at `target/cross-artifacts/sync-server` yourself (e.g. build on Linux x86_64 for an x86_64 VPS).

### VPS

- Tailscale joined; MagicDNS name or `100.x.x.x` for `DEPLOY_HOST`.
- Passwordless sudo, or run deploy from a terminal so `ssh -t` can prompt for sudo.
- Clients do not use SSH for Git — only smart HTTP on the same origin as the API.

## Configuration

```bash
cp deploy/deploy.env.example deploy/deploy.env
# DEPLOY_HOST, DEPLOY_USER, DOJJO_PUBLIC_URL, optional BUILD_STRATEGY / DOJJO_LISTEN / DEPLOY_TARGET
```

`deploy/deploy.env` is gitignored. Override path with `DOJJO_DEPLOY_ENV`.

Deploy-only variables (not read by `sync-server` at runtime):

| Variable | Default | Purpose |
|----------|---------|---------|
| `DEPLOY_HOST` | — | SSH target (required except `--build-only` without remote) |
| `DEPLOY_USER` | local `whoami` | SSH user |
| `BUILD_STRATEGY` | `auto` | `auto`, `zigbuild`, `cross`, or `remote` |
| `DEPLOY_TARGET` | from VPS `uname -m` | Rust triple; `--build-only` alone defaults to `x86_64-unknown-linux-gnu` |
| `DOJJO_INSTALL_BIN` | `/opt/dojjo/bin` | Binary install dir on VPS |
| `DOJJO_REMOTE_BUILD_DIR` | `dojjo-build` | Remote rsync dir (`remote` strategy only) |

Server runtime vars in the same file are written to `/etc/dojjo/sync-server.env` — see [SYNC_SERVER_DEPLOY.md](./SYNC_SERVER_DEPLOY.md).

## Build output

All strategies:

1. Run `./scripts/init-sync-server-db.sh` (local `DATABASE_URL=sqlite://…/sync-server/data.db` for compile-time SQL only; VPS paths are pinned separately before install).
2. Produce `target/cross-artifacts/sync-server` (Linux ELF from `target/<triple>/release/sync-server` or VPS build + scp).

Target triple: `DEPLOY_TARGET` if set; else `ssh … uname -m` on the VPS → `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`.

## Deploy

```bash
./scripts/deploy-sync-server.sh              # build + install + restart
./scripts/deploy-sync-server.sh --build-only # artifact only → target/cross-artifacts/
./scripts/deploy-sync-server.sh --install-only  # install existing artifact (needs DEPLOY_HOST)
```

Install phase (after build, unless `--build-only`; always over SSH to the VPS):

1. `deploy/remote/bootstrap.sh` — `dojjo` user, `DOJJO_INSTALL_BIN`, `DOJJO_DATA_DIR`, `git` (+ ssl libs) on Debian-like hosts.
2. Write `/etc/dojjo/sync-server.env` from deploy.env (`DOJJO_LISTEN` defaults to `$(tailscale ip -4):3000` on the VPS when unset).
3. Copy binary to `/tmp/sync-server.new`, install unit `deploy/systemd/dojjo-sync-server.service` → `/etc/systemd/system/`, `systemctl enable --now`.
4. Health probe: `GET http://<DOJJO_LISTEN>/api/dojo/__dojjo_deploy_probe__` (404/405 counts as up; `/` is 404).

Production VPS installs should set `DOJJO_PUBLIC_URL` in deploy.env (defaults to `http://${DEPLOY_HOST}:3000` when unset). Omit only for local/e2e.

## Network

Bind **`DOJJO_LISTEN`** to the Tailscale IPv4 (e.g. `100.64.0.1:3000`), not `0.0.0.0:3000`, so the service is not on the public NIC. Parsed in `sync-server/src/main.rs`; binds exactly that address.

Optional host firewall (example):

```bash
sudo ufw deny 3000
sudo ufw allow in on tailscale0 to any port 3000 proto tcp
```

Client after deploy:

```bash
dojjo init --api-base "${DOJJO_PUBLIC_URL}/api"   # DOJJO_PUBLIC_URL must include http(s)://; path must end with /api
```

Create/join return Git remotes `{DOJJO_PUBLIC_URL}/git/<dojo-id>.git` (smart HTTP, same process as API).

## Operations

```bash
ssh "$DEPLOY_USER@$DEPLOY_HOST" sudo systemctl status dojjo-sync-server
ssh "$DEPLOY_USER@$DEPLOY_HOST" sudo journalctl -u dojjo-sync-server -f
```

- Data: `DOJJO_DATA_DIR` (default `/var/lib/dojjo`) — per-dojo `mirror/`, `bare.git`, SQLite at `DATABASE_URL`.
- Migrations: applied on each `sync-server` start.
- Git smoke test: `git ls-remote "http://<host>:3000/git/<dojo-id>.git"`

## Troubleshooting

| Symptom | Likely cause |
|---------|----------------|
| `toolchain '…-x86_64-unknown-linux-gnu' may not run` | crates.io `cross` 0.2.5 — use zigbuild (Apple Silicon → x86_64), cross from git, or `remote` |
| cross SIGSEGV / QEMU | Apple Silicon building x86_64 via Docker — use `BUILD_STRATEGY=zigbuild` or `remote` |
| `Docker is not running` | Required for `cross` (including `auto` when not Apple Silicon → x86_64) |
| `missing command: cargo-zigbuild` / `zig` | Needed for zigbuild path only |
| compile / `sqlx` errors | Run `./scripts/init-sync-server-db.sh`; re-run deploy build |
| `bind DOJJO_LISTEN` | Wrong IP or Tailscale down — `tailscale ip -4` on VPS |
| `remote sudo needs a password but stdout is not a terminal` | Run from a real terminal or configure NOPASSWD sudo |
| Service up but probe fails | `journalctl -u dojjo-sync-server`; check `DOJJO_LISTEN` vs `ss -tlnp` |
| `git ls-remote` 404 | Wrong dojo id or missing `bare.git` under data dir |
| `git ls-remote` 500 | `git` missing on VPS (bootstrap should install) — check logs |
| Client `ssh://` remote | Rebuild `dojjo`; sync refreshes remote URL from API |
| Wrong DB path on VPS | Re-run `--install-only` so `/etc/dojjo/sync-server.env` has VPS `DATABASE_URL` (deploy pins server vars before build) |

## Docker

Optional container path — not used for the Tailscale VPS flow. See [SYNC_SERVER_DEPLOY.md](./SYNC_SERVER_DEPLOY.md#docker).
