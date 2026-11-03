# clouduler

Turn the machines you already own into one cluster that runs your Docker containers.

Describe an app once in a Compose-style file. Clouduler picks a machine, keeps its data replicated, puts it behind HTTPS on your domain, and moves it to another machine if the first one dies. One binary, no Kubernetes.

[Documentation](https://clouduler.liteapp.fr) · [Quick start](https://clouduler.liteapp.fr/docs/quickstart/single-node) · [UI tour](https://clouduler.liteapp.fr/docs/intro/tour)

<picture>
  <source media="(prefers-color-scheme: light)" srcset="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--view-tour--light.png">
  <img alt="Cluster overview: health, quorum, image updates, backups, certificates and nodes" src="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--view-tour--dark.png">
</picture>

## Why

It was built for clusters where the network is secure but unreliable and the nodes are very different from each other: a Raspberry Pi at home, a mini PC, a small VPS as public entry point. On that kind of setup it replaces Swarm + Traefik + GlusterFS/Ceph, with fewer parts to keep alive.

- **One binary** with the daemon, web UI and CLI. About 65 MB on arm64.
- **Few dependencies:** Docker, `restic` for backups, and the `nfsv4` kernel module for remote volumes.
- **Tested against failure.** Every crash found during development got a regression test, and a QEMU-based end-to-end harness replays failures on clusters of different sizes.

## What you get

### Stacks that look like Compose

Deploy from the UI, the CLI or git. Most behaviour is configurable per stack or per container, and Swarm-only features missing from Compose, such as configs, are supported.

<picture>
  <source media="(prefers-color-scheme: light)" srcset="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--stacks-nextcloud--light.png">
  <img alt="A stack with its containers, the node each runs on, and a rolling image update" src="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--stacks-nextcloud--dark.png">
</picture>

### Placement you can read

Every stack is placed by explicit constraints. When something cannot run, the table shows which constraint failed on which node, instead of a silent pending state.

<picture>
  <source media="(prefers-color-scheme: light)" srcset="https://clouduler.liteapp.fr/img/ui/node-down-failover--stacks-immich-tab-placement--light.png">
  <img alt="Placement table with one node unreachable" src="https://clouduler.liteapp.fr/img/ui/node-down-failover--stacks-immich-tab-placement--dark.png">
</picture>

### Replicated drives

A drive is a folder of volumes, replicated across the nodes that implement it. Data stays local, so a container that moves finds its files already there. Per-drive restic backups go to a node, SSH or S3.

<picture>
  <source media="(prefers-color-scheme: light)" srcset="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--storage-view-tour--light.png">
  <img alt="Replicated and served drives with a table of volumes and their sync state" src="https://clouduler.liteapp.fr/img/ui/lan-3-nodes--storage-view-tour--dark.png">
</picture>

### And also

- **Reverse proxy** with Let's Encrypt DNS-01 certificates through Cloudflare, routing each request to the right node.
- **Networks.** One public network and any number of private ones, each with its own subnet and domain. Someone outside the subnet cannot reach `home.example.com`.
- **Failover with fencing.** An isolated node stops its own containers before the cluster restarts them elsewhere, so a volume never has two writers.
- **GitOps.** Configuration is exported to git and can be restored without any data. Secrets are encrypted with age to a public key you provide.
- **Per-stack firewall.** A stack declares which networks and hosts it can reach, for example only `auth.example.com` and `*.github.com`.
- **Alerts** to Discord, webhook or email, and an optional **MCP server** so an assistant can inspect the cluster with secrets redacted.

## Quick start

On a Linux machine with systemd and Docker, install the binary from the [releases page](../../releases) and create the first node:

```
sudo install -m 0755 clouduler-$(uname -m)-unknown-linux-gnu /usr/local/bin/clouduler
sudo clouduler init --yes \
  --cluster-name home \
  --subnet 10.10.0.0/24 \
  --internal-domain int.example.com \
  --web-address cloud \
  --drive fast=/srv/clouduler/fast
```

The first start logs an admin API key once, so save it:

```
journalctl -u clouduler | grep -i -E "api key|fingerprint"
```

Then open `https://cloud.int.example.com:8443`, sign in with the key, and add more nodes from the Nodes page, which generates the join command for you. Use at least 3 managers for a fault-tolerant cluster.

The step-by-step guides cover everything from here:

- [Single node](https://clouduler.liteapp.fr/docs/quickstart/single-node): install, first stack, HTTPS
- [LAN cluster](https://clouduler.liteapp.fr/docs/quickstart/lan-cluster): three machines, replicated storage, failover
- [Multi-site with Headscale](https://clouduler.liteapp.fr/docs/quickstart/multi-site-headscale): home machines and a VPS

## Security model

The network between nodes is assumed to be secure: a LAN, WireGuard, Tailscale or Headscale. Raft, replication and overlay traffic are not encrypted by clouduler, but node-to-node administration traffic is authenticated. Do not expose the cluster subnet to untrusted networks.

Secrets are sealed with XChaCha20-Poly1305 under a cluster key that is only shared at pairing, encrypted to the joining node's public key. Back up `/etc/clouduler/secrets.key`: if every copy is lost, so are the stored secrets.

## Documentation

- [What is Clouduler](https://clouduler.liteapp.fr/docs/intro/what-is-clouduler) and [how it works](https://clouduler.liteapp.fr/docs/intro/how-it-works)
- [Features](https://clouduler.liteapp.fr/docs/features/stacks): stacks, drives, proxy, networking, failover, backups, alerts and more
- [Examples](https://clouduler.liteapp.fr/docs/examples): Nextcloud, Immich, Authentik, Vaultwarden, Forgejo, Headscale, Stalwart
- [Reference](https://clouduler.liteapp.fr/docs/reference/cli): CLI, `cluster.yaml`, stack spec, HTTP API, troubleshooting

## Building from source

```
cd ui && npm ci && npm run build && cd ..
cargo build --release -p clouduler
```

The docs site, including these screenshots, is built from `docs/` with `npm run build:full`. See [docs components](https://clouduler.liteapp.fr/docs/contributing/docs-screens).
