# Dojjo

[Jujutsu](https://github.com/jj-vcs/jj) (jj) workspaces over a network. Work in the same local jj repository across multiple devices, where each device gets its own workspace(s). Use regular jj commands and tooling; as far as jj knows, your repo is just a regular local repo.

Dojjo is orthogonal to `jj git push` / `jj git fetch`. You can still push and pull to remotes from any workspace on any machine.

> [!CAUTION]
> Dojjo is alpha software. Repository corruption is absolutely possible. Always back up your repository before using it with dojjo.

---

## What Dojjo is for

Jujutsu already supports multiple **workspaces** on one machine sharing one **repo** (one operation log, one view of history). Dojjo extends that idea across the network:

- One **dojo** on a **sync server** holds the authoritative shared repo state.
- Each **device** (laptop, desktop, CI runner, etc.) is a separate **client** with its own filesystem.
- Clients exchange state only through the server (and its bare Git remote where applicable). Nothing assumes a shared disk between peers.

The server is a **bridge**, not a live filesystem. Your machines stay isolated until sync moves data.

---

## Mental model: two machines, two repos, one dojo

```text
  Machine A                         Sync server                    Machine B
  ─────────                         ───────────                    ─────────
  ~/project-a/                      dojo "abc"                     ~/project-b/
    (your workspace)                  ├─ mirror: shared .jj/repo     (your workspace)
                                    └─ bare git: object transport      (checkout via join)
  local repo
  ~/.dojjo/.../abc/_default/
    physical .jj/repo  ──push/pull──►  canonical copy  ◄──push/pull──  physical .jj/repo
```

**What this means:**

1. **Each device is its own client** — separate config home (`DOJJO_HOME`), separate project directory, separate **local repo** (physical `.jj/repo`) on disk.
2. **Machines do not share storage** — you only see each other's changes after `dojjo` push/pull through the server.
3. **After sync, history matches** — same JJ operations, same workspace names in the shared view, same commit graph — while **your working-copy files and paths** stay local on each machine.

Two JJ workspaces in the **same** local repo directory on one machine are not two devices sharing a dojo over the network.

---

## Roles

| Role | Who | Responsibility |
|------|-----|----------------|
| **Sync server** | Hosted dojo | Stores shared repo blobs (JJ mirror + bare Git for object transport). Does not edit history. |
| **Creator** | First client | Runs `dojjo create` from an existing JJ project; publishes that repo as a new dojo. |
| **Joiner** | Every other client | Runs `dojjo join` into an **empty** project directory; receives a replica of shared repo state and a new workspace identity. |

After create or join, every client stays linked and syncs in the background (see [What happens on each sync](#what-happens-on-each-sync)).

For now, Dojjo assumes all clients and the server are fully trusted. Replication is last-writer on blobs; JJ reconciles history locally on each machine.

---

## Shared vs local

**Shared through the dojo**

- Commit / object backend data needed to interpret history.
- Operation log and views (who moved which bookmark, which workspace's `@` points where).
- Op heads and related repo metadata the JJ loader expects.
- Git-backed object data via the dojo's bare remote (not as naive per-file `.git/objects` mirror blobs).

**Local to each machine (never shared)**

- Absolute paths in `workspace_store` (which directory on *this* host backs workspace `alice`).
- Per-workspace working-copy internals under `.jj/working_copy` (trees, caches, locks).
- Local Git working tree state that JJ manages beside the repo (except what JJ explicitly stores in the shared repo).

After pull, you may need normal JJ commands (e.g. `jj workspace update-stale`) so **your** `@` matches the updated shared view. That is local housekeeping, not another server round trip.

---

## `dojjo create` (first peer only)

**When to use it:** "I have a JJ project; turn it into a dojo others can join."

**Before you run it**

- Run from a normal JJ workspace (the project you want to share).
- That workspace becomes your checkout; `create` does not make a new one for you.

**What happens**

1. Allocate a new dojo on the server (id, API base, bare Git remote URL).
2. Establish the **local repo** for this dojo on your machine (physical `.jj/repo` under `_default` in dojo home).
3. Re-home your repo so the physical store lives in the local repo; your project dir keeps a **pointer** into that store (same JJ multi-workspace pattern as on one machine).
4. Reserve the shared workspace name `default` on the server side: frozen, sparse-empty, `@` on `root()` — a sentinel in the shared view, not where humans edit.
5. Give you a **non-`default` workspace name** in the shared view (e.g. hostname).
6. Publish initial shared state to the server (JJ mirror + Git push).
7. Link your project `.jj` to this dojo (so later sync knows which dojo to use).

**If you interrupt or need to retry**

- Safe to re-run `dojjo create` in the same workspace after Ctrl-C or a crash.
- Progress is stored in `~/.dojjo/dojos/{id}/create_progress.json` and `.jj/dojjo.json` (written right after the server allocates the dojo).
- Re-run skips completed phases: re-home (if already pointing at `_default`), `git push`, and mirror uploads already on the server (by manifest hash).
- If create already finished (`config.json` exists), re-run prints a message and exits successfully; use `dojjo dev sync` instead.

**What create is not**

- Not "join my friend's dojo."
- Not "run from an empty directory with no JJ yet" (you already have a project).

---

## `dojjo join` (every other peer)

**When to use it:** "I want a checkout of this dojo on **this** machine in **this** directory."

- **First time** this dojo exists on the machine: cold join (download from server, then add workspace).
- **Already** have a linked workspace or finished `create` for this dojo here: warm join — same command, but only adds a workspace locally (no re-download).

**Before you run it**

- `--into` is an **empty** directory (no `.jj`, no existing JJ repo there).
- You have a dojo id (and server URL).
- You do **not** need an existing JJ repo anywhere on the machine for a **cold** join; for a **warm** join the dojo must already be set up under `DOJJO_HOME`.
- Do **not** run join from a directory that is already a JJ workspace for this or another project — join is not "attach my existing repo to a dojo." That conflates two repos and bypasses the bridge.

**What happens**

**Cold join** (first time this dojo is set up on this machine):

1. Fetch dojo metadata from the server (id, Git remote URL, etc.).
2. Create the local repo layout under this machine's `DOJJO_HOME` for that dojo.
3. Download shared repo state from the server into this machine's local repo.
4. Fetch Git objects from the dojo bare remote so Git-backed repos are usable locally.
5. Create a new JJ workspace at `--into` backed by the local repo (new workspace name in the shared view, unique across the dojo).
6. Link `--into/.jj/dojjo.json` so sync commands resolve the right server and local repo.

**Warm join** (dojo already set up on this machine):

Dojjo runs `jj workspace add` against the existing local repo. The practical benefit over running `jj workspace add` yourself is that you only need the **dojo id** and an empty `--into` (from a neutral directory). You do not need to `cd` into an existing linked checkout or know where `_default` lives under `DOJJO_HOME`.

If you are already in a linked workspace, plain `jj workspace add` is equivalent for repo behavior.

**What join is not**

- Not `jj clone` from another directory on the same machine.
- Not "run from the creator's project tree so `jj -R` shares their repo."
- Not a substitute for `dojjo dev sync` when you need this machine's local dojo replica to catch up with the server (all workspaces share the same local repo until sync runs).

**Workspace naming**

- Each join chooses a **globally unique** workspace name within the dojo (collision = error).
- Do not use the reserved name `default` for human workspaces.

---

## What happens on each sync

After `create` or `join`, Dojjo keeps **this machine's local dojo replica** in sync with the server in the background. Each sync round makes your local repo match the server and publishes your new repo changes to it.

Background sync is enabled by default on first `create`/`join` for a dojo on a machine. Each background sync round is equivalent to running `dojjo dev sync` manually. You usually do not need to run that command yourself; use it to debug sync problems or force a sync when you want one immediately. Disable background sync for troubleshooting with `dojjo background-sync disable`.

**Scope**

- Sync runs from a workspace **linked** to the dojo (`dojjo.json` or equivalent).
- Sync always read/writes the **local repo** for that dojo on **this** machine, not someone else's disk.

**Each round**

1. **Git leg (if applicable):** push local Git-backed objects to the dojo bare remote; fetch missing objects from the server before applying JJ mirror updates.
2. **JJ leg:** compare this machine's local repo files to the server manifest; upload changed mirrored files; download files the server has that differ locally; apply in dependency-safe order.
3. Leave machine-local paths (`workspace_store`, git transport scratch, working copies) out of the contract — local copies may exist without being on the server.
4. After sync, your workspace should be able to see merged history (possibly after JJ's own `update-stale` / reconcile steps).

**What sync is not**

- Not editing files in the server's frozen `default` workspace on disk.
- Not rsync of the whole project directory (only repo state crosses the bridge; working tree files are edited locally).
- Not a substitute for join on a machine that has never pulled a local dojo replica.

---

## What to expect

1. **One dojo, many devices, many workspace names** — each join adds one name in the shared view; each device keeps its own checkout path.
2. **`default` is server convention only** — use other names for yourself; `default` stays frozen in the shared view.
3. **Join is cold-start for `--into`** — empty directory, no pre-existing JJ repo there.
4. **Sync is local dojo replica ↔ server** — never assume another peer's filesystem is visible.
5. **JJ reconciles, Dojjo transports** — conflicts and op-head merging are JJ's job after faithful replication.
6. **Workspace paths are local** — syncing them would leak host filesystem layout and break other machines.

---

In one sentence: **each machine keeps its own local dojo replica and workspace checkout; the dojo server is the only wire between them.**
