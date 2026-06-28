
# Kogiasima 🐳

> **Kogiasima** is named after the *Dwarf Sperm Whale (Kogia sima)*, one of the smallest whales in the ocean. Inspired by Docker and its famous mascot Moby Dock, Kogiasima is a highly educational, low-level container runtime built from scratch in Rust to explore how container sandboxing actually works down to the bare Linux kernel primitives.

---

## Core Features

### 1. Automated OCI Specification Generator

Kogiasima features a built-in CLI compiler that dynamically spits out an  `config.json`. You can easily tune resource walls, target environments, and filesystem properties straight from the command line:

* **Custom Execution:** Set isolated arguments (`--args`), runtime directory structures (`--cwd`), and natively inject environmental maps (`--env`).
* **Namespace Toggling:** Granular control over which kernel barriers are activated (`pid`, `network`, `mount`, `uts`).

### 2. Deep Namespacing & File Tree Isolation

* **`pivot_root` Engine:** Safely swaps the host operating system tree for a designated rootfs (like Alpine Linux), separating mount tracking by moving root boundaries into `MS_PRIVATE` isolation states.
* **Namespace Isolation:** Decouples tasks across `CLONE_NEWPID`, `CLONE_NEWNS`, `CLONE_NEWUTS`, and `CLONE_NEWNET` boundaries using a compliant double-fork sequence to guarantee host tracking cannot leak into grandchild workflows.

### 3. Native Netfilter (nftables) & Veth Bridging

* **Asynchronous Link Provisioning:** Provisions kernel virtual ethernet pairs (`veth`) and bridges (`br0`) using `rtnetlink` via a multi-threaded asynchronous Tokio executor runtime.
* **Stateful Firewall Hooks:** Injecting Netfilter rules over raw sockets directly into kernel namespaces using base chains. Features automatic Masquerading (`srcnat`), connection state tracking (`established, related`), and isolation filtering policies to secure incoming/outgoing traffic boundaries.

### 4. Linux Control Groups (cgroups v2)

* **Resource Hardlining:** Restricts container workloads by binding running tasks inside the modern unified cgroup v2 tree layer (`/sys/fs/cgroup/`).
* **Dynamic Boundary Limits:** Controls memory utilization limits (`memory.max`) and restricts internal thread growth (`pids.max`) to protect your host against fork-bomb exploits.

### 5. Production Diagnostics & Tracing

* **Structured System Diagnostics:** Integrated tracking via the `tracing` ecosystem utilizing key-value structured event frames for predictable log evaluation.

---

## Usage Guide

### Generate an OCI Configuration

To compile a custom `config.json` specification directly into your container bundle root:

```bash
sudo cargo run -- \
  --args "/bin/sh" \
  --hostname "kogiasima-box" \
  --pid-limit 30 \
  --memory-limit 104857600 \
  --env DEBIAN_FRONTEND=noninteractive \
  --output "/home/aayush/alpine_bundle/config.json"

```

### Run the Container Engine

Once your `config.json` is generated and a valid `rootfs` layout is prepared, kick off the runtime:

```bash
# Run with tracing diagnostics level active
sudo RUST_LOG=info cargo run

```

---

## CLI Flag Glossary

| Option | Default | Example / Description |
| --- | --- | --- |
| `--args <CMD>` | `/bin/sh` | Command string to pass inside the spawned payload. |
| `--env <K=V>` | *None* | Extra environmental values (Repeatable: `--env A=1 --env B=2`). |
| `--cwd <PATH>` | `/` | Execution working directory tracking inside the rootfs. |
| `--hostname <NAME>` | `mini-docker-isolated` | Hostname mapped inside the new UTS namespace loop. |
| `--rootfs <PATH>` | `rootfs` | Path on the host pointing to the root filesystem bundle directory. |
| `--readonly` | `false` | Instructs the engine to clamp down the target rootfs as read-only. |
| `--namespace <TYPE>` | `pid network mount uts` | Namespaces to detach (Options: `pid`, `network`, `mount`, `uts`, `ipc`, `user`). |
| `--pid-limit <N>` | `20` | Max allocation threshold for running pids inside the cgroup constraint. |
| `--memory-limit <B>` | `104857600` | RAM utilization wall in bytes (Defaults to 100MB). |
| `--output <PATH>` | `config.json` | Where the final OCI specification map should save. |

---

## Architecture Design Flow

1. **Parse & Verify:** The engine consumes your generated `config.json` map and checks the host's `net.ipv4.ip_forwarding` kernel toggles.
2. **First Fork & Unshare:** The first child splits away, instantly unsharing the isolated namespace boundaries (`CLONE_NEWNS`, etc.).
3. **Host Optimization:** While the child safely suspends on a synchronization pipe, the host hooks into the active parent PID to structure the cgroup resource tracks, wire up the asymmetric `veth` cables to `br0`, and broadcast `nftables` netlink rules into the kernel.
4. **Second Fork & Pivot:** The child cuts its root propagation line via `MS_PRIVATE`, triggers a secondary fork to trap the definitive PID constraints, invokes `pivot_root` to swap file systems, and gracefully replaces its thread image with your target program.

```

```
