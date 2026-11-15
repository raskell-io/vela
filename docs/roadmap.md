# Vision, Strategy and Roadmap

Vela is the scheduler for people who own their machines. This page says where it is going, what stays fixed on the way, and in which order. It is a plan, not a promise: open an issue to argue with any part of it.

## Where Vela stands today

Vela 0.6 runs any number of apps and their services on one Linux server you own, with zero-downtime deploys over SSH, from one binary and with no containers. It is used in production and works well because it does few things.

| Area | What exists (v0.6) |
| --- | --- |
| Deploys | Blue-green and sequential swaps, health-gated, `current` symlink as the atomic switch, rollback |
| Workloads | Rust binaries, BEAM releases, any executable; remote builds from source |
| Services | Built-in Postgres and NATS; any other command as a supervised sidecar (`[services.<name>] command = ...`) |
| Supervision | Auto-restart of crashed apps and services, restore on daemon restart |
| Edge | Embedded proxy, ACME certificates per domain (also on activation and SIGHUP), static certs, mTLS for Cloudflare origin pulls, WebSockets |
| Operations | Secrets, scheduled backups, pre/post deploy hooks, `vela status --json` |
| Isolation | Read-only release directories and a hardened systemd unit; apps and services still run as root |

What it deliberately does not do: containers or images, a REST or gRPC control API, its own auth, a database for its own state, or more than one machine. The last one is the boundary this page proposes to move; the others stay.

## Vision

**Vela is the scheduler for people who own their machines: one binary that runs any workload on one to a few dozen servers, with nothing to operate besides SSH and the filesystem.**

Nomad proved that one binary, one job file and a few workload types beat a platform built from dozens of parts. Vela takes that idea further down: no cluster consensus to babysit, no separate service mesh, no image registry, no ACL system. A small team should be able to read the whole state of their fleet with `ls`, `cat` and `readlink`, and fix it by hand when something goes wrong.

What "more versatile" means for Vela:

- **More kinds of work.** Long-running apps today; also batch jobs, scheduled jobs, one-off tasks, and services such as Postgres, Redis or Meilisearch declared like apps.
- **More than one machine.** The same `Vela.toml` deploys to one server or spreads instances across several, and the proxy on every node knows where each app runs.
- **More ways to get a release in.** Prebuilt binaries, BEAM releases, builds on the server, and plain directories of files, still never an image.
- **Same feel at every size.** A one-server user should never pay for multi-server features in config, memory or concepts.

## Principles that bound the expansion

Every new feature has to pass these seven checks; a feature that fails one is redesigned or dropped.

1. **One binary, one config language.** No agents, sidecars or plugins to install separately. TOML for everything a person writes.
2. **The filesystem is the database.** Desired and actual state are files and symlinks a person can read and repair. If a multi-node feature needs shared state, it is a file replicated over SSH, not a consensus log.
3. **SSH is the only control plane.** Who can SSH (and `sudo vela`) can operate. No tokens, no API server, no web UI required.
4. **Processes, not containers.** Isolation comes from Linux: users, namespaces, cgroups v2, systemd-style sandboxing.
5. **Pay only for what you use.** A feature that is not configured costs no memory, no background task and no config line.
6. **Fail loudly, keep the last good state.** A failed deploy, placement or migration leaves what was running untouched.
7. **Small enough to read.** A new concept must be explainable in one paragraph of the docs; the codebase should stay readable in an afternoon (about 8,500 lines of Rust, tests included, at v0.6).

## Strategy

Take Nomad's workload model and refuse its distributed-systems machinery. Nomad's value is that one job file describes any kind of work; its cost is Raft servers, gossip, ACLs and a Consul/Vault orbit. Vela can keep the first and avoid the second by treating each server as independent and letting the client coordinate over SSH.

| Nomad idea | Vela's answer | Why |
| --- | --- | --- |
| Job types: service, batch, system, periodic | Borrow: `kind = "service" \| "job" \| "cron"` in `Vela.toml` | Biggest gain in versatility for the least new code |
| Task drivers (exec, java, docker) | Borrow the idea, not the plugin system: built-in `binary`, `beam`, `exec` types | Covers real workloads without a plugin ABI |
| Rolling and canary updates | Borrow: extend blue-green to `canary = 1` then promote | Natural next step from today's swap logic |
| Constraints and spread | Borrow a small subset: name nodes or set `count`; the client spreads | Placement without a scheduler service |
| Templates and env from Vault/Consul | Replace with `${secret:KEY}` and service `export`s | Already in place for apps and services |
| Raft server cluster, gossip | Refuse: nodes stay independent, the fleet is a file of hosts | No quorum to lose, nothing to babysit |
| ACLs, namespaces, tokens | Refuse: SSH keys and sudoers are the access model | Existing Unix tooling does it |
| Docker driver, CSI volumes | Refuse: processes and local directories only | Core promise of the project |
| Web UI | Refuse in core; `vela status --json` feeds any dashboard | Keeps the binary small |

Four design moves carry the roadmap:

1. **Everything is a workload.** An app is a workload with a domain; a sidecar service is a workload without one; a job is a workload that exits. One supervisor, one release layout, one status view.
2. **The fleet is a file.** `Vela.toml` may list several `servers`. The client deploys to each over SSH and stops on the first failure. Each node keeps its own filesystem state, so a node can always be inspected and repaired alone.
3. **Routing follows placement, not discovery.** Each node's proxy routes to local instances; an optional `peers` list lets a node forward to another node over a private network (WireGuard), so DNS can point at any node.
4. **Isolation hardens in place.** cgroups v2 limits from `[resources]`, then mount and PID namespaces, all opt-in per app and all with plain processes underneath.

## Roadmap

Five phases from v0.6 to v1.0, each closed by a gate that a real deployment has to pass. No dates: the order matters more than the calendar. Multi-server work waits until single-server workloads, limits and canaries are solid.

| Phase | Scope | Gate to the next phase |
| --- | --- | --- |
| **v0.6 · Every workload** | Generic sidecar services in `Vela.toml` (#12, shipped in 0.6.0) · `job` and `cron` kinds for one-off and scheduled work · `exec` type: any command, not only a release binary | Meilisearch runs under Vela next to an app |
| **v0.7 · Safer swaps, real limits** | Canary deploys: one new instance, then promote · cgroups v2 memory and CPU limits from `[resources]` · log rotation; jobs and services in `vela status` | Limits enforced and canary rollback tested |
| **v0.8 · The fleet is a file** | Several servers in `Vela.toml`, deployed one after another · `count = N` instances spread across the listed servers · `vela status` across every server of an app | One app on two servers; a failed node halts the deploy |
| **v0.9 · Routing and isolation** | Optional forwarding between nodes over WireGuard · opt-in mount and PID namespaces per app, with a separate user per app · health-aware routing across nodes | Any node answers for any app in the fleet |
| **v1.0 · Stable** | `Vela.toml` format frozen, upgrade notes for every change · one docs page per concept, each read in five minutes · binary size and memory budget checked in CI | No breaking change for three months |

## Non-goals

So they do not creep back in:

- Containers, OCI images, or a Docker or Podman driver
- A consensus cluster, leader election or automatic rescheduling onto other nodes after a node dies
- A REST or gRPC API, a web UI in the core binary, or its own user and token system
- Hundreds of nodes; the target is one to a few dozen servers
- Windows or macOS as server platforms

## Open questions

- Should generic sidecar services replace the built-in Postgres and NATS providers, or sit beside them? The built-in providers know backups and connection URLs; generic ones are more versatile. Today they sit side by side.
- Is cross-node forwarding worth building, or is "point DNS or Cloudflare at every node that runs the app" enough for the target users?
- Should `vela` keep a fleet file on the laptop (`fleet.toml`) or only in each project's `Vela.toml`?
- How far should isolation go before it stops being "just processes"? A clear line between namespaces and a container would help the docs.
- Release cadence: one minor version per phase, or smaller releases per feature?
