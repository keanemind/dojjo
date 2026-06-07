# Development setup

Dojjo is Rust (`dojjo`, `sync-server`, `dojjo-mirror`) plus bash/Python e2e scripts. [mise](https://mise.jdx.dev/) pins tool versions and wraps common commands.

## Prerequisites

Install mise and enable shell activation (once per machine):

```bash
curl https://mise.run | sh
# follow the printed hook instructions for your shell (e.g. ~/.bashrc, ~/.zshrc)
```

`git` must be on `PATH` (system install is fine; e2e and sync-server invoke `git` directly).

## First-time setup

From the repo root:

```bash
mise trust      # once per clone: allow this repo's mise.toml
mise install    # rust, python, jj 0.38.x, sqlx-cli
mise run setup  # sync-server sqlx offline DB
```

`mise install` activates pinned tools when you `cd` into this directory.

The first `mise install` may compile `sqlx-cli` from source if no prebuilt binary exists (~2–3 minutes).

### Why jj 0.38.0?

`jj-lib` in `sync-server` and `dojjo-mirror` is pinned to **0.38.0**. E2e scripts drive the `jj` CLI against real repos; keep the CLI aligned with the library (mise overrides a newer system `jj`).

### JJ reference tree

For reading jj library/CLI source while working on dojjo (AI agents, debugging `jj-lib` behavior):

```bash
mise run fetch-jj-ref
```

This shallow-clones tag `v{jj_version}` from [jj-vcs/jj](https://github.com/jj-vcs/jj) into `jj-<commit-sha>/` at the repo root. The tree is local-only (see `.gitignore`); jj ignores it as a nested git repo.

When bumping `jj_version`: update `mise.toml`, `jj-lib` in Cargo.toml files, run `mise install`, then `mise run fetch-jj-ref`. Delete stale `jj-*` dirs manually when no longer needed. Re-audit `dojjo-mirror/src/mirror_exclude.rs` `FileLock` paths against the new tree.

## Common tasks

| Command | What it does |
|---------|----------------|
| `mise run init-db` | `scripts/init-sync-server-db.sh` (required before first Rust build) |
| `mise run fetch-jj-ref` | Shallow-clone pinned jj tag into `jj-<sha>/` for source reference |
| `mise run setup` | `init-db` (sqlx offline DB) |
| `mise run build` | `cargo build --bin dojjo --bin sync-server` (runs `init-db` first) |
| `mise run test` | `cargo test --workspace` |
| `mise run e2e:mvp` | Two-peer smoke test |
| `mise run e2e:composite` | Composite git+jj sync acceptance |
| `mise run e2e:cold-join-extra-heads` | Extra table heads regression |
| `mise run e2e:sync-safety` | Chaos / safety (slow; needs `rsync` on PATH) |
| `mise run e2e:idle-sync-no-new-op` | Idle sync gate |
| `mise run e2e:idle-sync-stress` | Idle sync stress (40 rounds default) |
| `mise run e2e:all` | MVP + composite + cold-join + idle-no-new-op |

List everything: `mise tasks`

## Project layout

| Path | Role |
|------|------|
| `client/` | `dojjo` CLI |
| `sync-server/` | HTTP mirror + git smart HTTP |
| `dojjo-mirror/` | Shared mirror path/exclude logic |
| `scripts/e2e-*.sh` | Integration tests (two simulated `DOJJO_HOME` peers) |

Two-peer harness (`DOJJO_HOME_A` / `DOJJO_HOME_B`, cold join): see [README.md](../README.md) and `scripts/e2e-*.sh` headers.

## CI

Use the same `mise.toml` in GitHub Actions (`jdx/mise-action`) so CI and local dev share pins. Example:

```yaml
- uses: jdx/mise-action@v2
- run: mise run test
- run: mise run e2e:all
```

## Debug-only e2e scripts

Not wired as mise tasks (ad-hoc investigation):

- `scripts/e2e-idle-sync-prove.sh`
- `scripts/e2e-idle-sync-investigate.sh`

Run manually when debugging idle-sync churn (`scripts/e2e-idle-sync-prove.sh`, `scripts/e2e-idle-sync-investigate.sh`; gate: `mise run e2e:idle-sync-stress`).
