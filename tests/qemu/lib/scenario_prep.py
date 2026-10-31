#!/usr/bin/env python3
"""Reads a scenario yaml file, allocates host ports, writes the generated
cluster.yaml for the run, and prints a JSON summary for the bash driver to
consume with jq. Kept separate from the bash side because parsing yaml and
picking free ports is much less painful in python than in bash.
"""
import base64
import json
import os
import secrets
import socket
import stat
import sys
import uuid

import yaml

DEFAULT_FAILOVER = {"probe_interval": "2s", "fence_timeout": "6s", "node_timeout": "12s"}


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main():
    if len(sys.argv) != 5:
        print(
            "usage: scenario_prep.py <scenario.yaml> <out-cluster.yaml> <out-control.yaml> <mcast-id>",
            file=sys.stderr,
        )
        sys.exit(2)
    scenario_path, out_cluster_path, out_control_path, mcast_id = sys.argv[1:5]

    with open(scenario_path) as f:
        scenario = yaml.safe_load(f)

    nodes = scenario["nodes"]
    failover = {**DEFAULT_FAILOVER, **scenario.get("failover", {})}
    drives_cfg = scenario.get("drives", {})

    # cluster-wide drives block: whatever the scenario declares, plus the
    # union of anything referenced per-node in case the scenario left it implicit.
    all_drive_names = set(drives_cfg.keys())
    for n in nodes:
        all_drive_names.update((n.get("drives") or {}).keys())
    drives_block = {}
    for name in sorted(all_drive_names):
        entry = {"sync_interval": drives_cfg.get(name, {}).get("sync_interval", "30s")}
        served_by = drives_cfg.get(name, {}).get("served_by")
        if served_by:
            entry["mode"] = "served"
            entry["served_by"] = served_by
        drives_block[name] = entry

    # vpn_segment: an isolated second network segment simulating a VPN
    # tunnel, only present when a scenario declares it. `egress_node` gets a
    # host interface (eth2) named in cluster.yaml as an egress network's
    # node address; `canary_node` gets the same segment and a fixed-response
    # listener, standing in for something only reachable through the tunnel.
    # See docs/networking.md's "Egress networks" section for what this is
    # actually testing.
    vpn = scenario.get("vpn_segment")
    vpn_ips = {}
    egress_networks_block = {}
    if vpn:
        vpn_ips[vpn["egress_node"]] = "172.31.0.1/24"
        vpn_ips[vpn["canary_node"]] = "172.31.0.2/24"
        egress_networks_block[vpn["network"]] = {"type": "egress"}

    extra_networks = scenario.get("networks") or {}

    node_summaries = []
    # Built for every node, including ones that `join: true` marks as joining
    # after the cluster is already up - `cluster_nodes`/`control_nodes` below
    # are the subset actually written into the run's initial config; the full
    # dict is what a later `join_node` step pushes via `config reload` and
    # bakes onto the joining node's own disk, so its id and address exist
    # exactly once and match everywhere they're used.
    full_cluster_nodes = {}
    join_names = {n["name"] for n in nodes if n.get("join")}
    for index, n in enumerate(nodes):
        name = n["name"]
        # Minted here rather than left to `clouduler init` (which is not part
        # of this harness): `id` is required by the config schema
        # (`cl_config::Node`) and used for raft membership identity, so every
        # node needs one whether it boots immediately or joins later.
        node_id = str(uuid.uuid4())
        web_port = free_port()
        rpc_port = free_port()
        # The two ports a published service answers on inside the guest: the
        # reverse proxy's plain listener, and the one published port the
        # publishing scenario uses. Forwarded like the api's, so an assertion
        # can be made from the host without a route onto the cluster network.
        http_port = free_port()
        expose_port = free_port()
        # Each node gets its own loopback address on the host. They all route
        # to lo, but a config needs every node on a distinct address inside the
        # declared subnet, and qemu binds its forwarded ports per address, so
        # one 127.0.0.1 for all of them would be rejected by both.
        control_ip = f"127.0.0.{index + 1}"
        node_drives = n.get("drives") or {}
        # 203.0.113.0/24 (RFC 5737 TEST-NET-3): not routable, but a public
        # network address just needs to exist for placement/gating purposes
        # in these scenarios, since nothing here does real DNS/ACME with it.
        node_network = {"cluster": n["ip"], "wan": {"ipv4": f"203.0.113.{index + 1}"}}
        # Scenario-declared extra networks (`networks:` at the top level) this
        # node sits on, as `addresses: {lan: 192.168.50.1}`. Each such address,
        # and the wan one when `wan_on_mesh` is set, is also configured on the
        # node's cluster nic (eth0) as an alias, so VMs can reach each other
        # "on" those networks and a client's source address is the one of the
        # network it used. See lib/guest-init.sh's `claliases`.
        aliases = []
        for net_name, addr in (n.get("addresses") or {}).items():
            node_network[net_name] = addr
            prefix = extra_networks[net_name]["subnet"].split("/")[1]
            aliases.append(f"{addr}/{prefix}")
        if scenario.get("wan_on_mesh"):
            aliases.append(f"203.0.113.{index + 1}/24")
        if vpn and name == vpn["egress_node"]:
            # A bare non-IP string is the third `NodeAddress` form: a host
            # interface name, naming eth2 (the vpn-segment netdev) rather
            # than an address on it.
            node_network[vpn["network"]] = "eth2"
        full_cluster_nodes[name] = {
            "id": node_id,
            "role": n["role"],
            "network": node_network,
            "rpc_port": 7373,
            "drives": {d: f"/srv/clouduler/{d}" for d in node_drives},
        }
        node_summaries.append({
            "name": name,
            "role": n["role"],
            "ip": n["ip"],
            "web_port": web_port,
            "rpc_port": rpc_port,
            "http_port": http_port,
            "expose_port": expose_port,
            "control_ip": control_ip,
            "drives": list(node_drives.keys()),
            "vpn_ip": vpn_ips.get(name, ""),
            "vpn_canary": "1" if vpn and name == vpn["canary_node"] else "",
            "join": name in join_names,
            "aliases": ",".join(aliases),
        })

    # One cluster key for the whole run, generated here and installed into
    # every node's image. Letting each node generate its own (the default for
    # a first launch) gives three managers three different keys, and a secret
    # sealed by the leader is then unreadable everywhere else. The host copy
    # is what lets the cli deploy a stack carrying secrets.
    key_path = os.path.join(os.path.dirname(os.path.abspath(out_control_path)), "secrets.key")
    with open(key_path, "w") as f:
        f.write(base64.b64encode(secrets.token_bytes(32)).decode() + "\n")
    os.chmod(key_path, stat.S_IRUSR | stat.S_IWUSR)

    # control.yaml's per-node dict, same shape as `full_cluster_nodes` but every
    # address points at its host-forwarded loopback address and port, since the
    # CLI runs on the host and has no route onto the VMs' own cluster network.
    # Only ever the nodes booted at t=0: the cli only ever needs to reach an
    # already-running manager, never a node still waiting to join, and a
    # `join_node` step pushes its config change through the leader's own api
    # instead (see steps.sh) - control.yaml's addresses are host-loopback
    # ones, meaningless payload content for the *guest* network the joining
    # node and its new peers actually talk over.
    control_nodes = {}
    for ns in node_summaries:
        if ns["join"]:
            continue
        control_network = {"cluster": ns["control_ip"]}
        if vpn and ns["name"] == vpn["egress_node"]:
            control_network[vpn["network"]] = "eth2"
        control_nodes[ns["name"]] = {
            "id": full_cluster_nodes[ns["name"]]["id"],
            "role": ns["role"],
            "network": control_network,
            "rpc_port": ns["rpc_port"],
            "drives": {d: f"/srv/clouduler/{d}" for d in ns["drives"]},
        }

    def build_cluster(nodes_block, internal_subnet, key_file):
        return {
            "cluster": {"name": scenario.get("name", "qemu-test")},
            "networks": {
                "cluster": {"type": "internal", "subnet": internal_subnet, "domain": "cl.test"},
                # A route on a subnetted network only answers a TCP peer inside
                # that subnet (docs/proxy.md, "Network gating"); the http_port
                # forward the publishing scenario asserts against arrives over
                # each VM's SLIRP nic instead, which is outside "cluster". A
                # `public` network has no subnet and so no gating, which is also
                # the correct real-world choice for anything meant to be
                # reachable from outside the cluster, so stacks that publish an
                # externally-facing route use this instead of "cluster".
                "wan": {"type": "public", "domain": "wan.test"},
                **egress_networks_block,
                **extra_networks,
            },
            "drives": drives_block,
            "nodes": nodes_block,
            "web": {"address": "api", "network": "cluster", "port": 8443},
            "failover": failover,
            "secrets": {"key_file": key_file, "generate_if_missing": False},
            **({"proxy": scenario["proxy"]} if scenario.get("proxy") else {}),
        }

    # Nodes marked `join: true` are left out of the run's *initial* config -
    # scenario-run.sh only boots the rest at t=0. The "-joined" cluster.yaml
    # carries every node, including those, and is what a `join_node` step
    # later bakes onto the joining node's own disk. Its json twin is the
    # payload a `join_node` step PUTs to the leader's own `/api/config`
    # (see steps.sh) - real guest (10.10.0.x) addresses throughout, unlike
    # control.yaml's host-loopback ones, which only the cli (running on the
    # host) needs and which would be meaningless to every other guest.
    joined_cluster_path = f"{os.path.splitext(out_cluster_path)[0]}-joined.yaml"
    joined_cluster_json_path = f"{os.path.splitext(out_cluster_path)[0]}-joined.json"

    base_cluster_nodes = {n: v for n, v in full_cluster_nodes.items() if n not in join_names}
    full_cluster = build_cluster(full_cluster_nodes, "10.10.0.0/24", "/etc/clouduler/secrets.key")

    with open(out_cluster_path, "w") as f:
        yaml.safe_dump(build_cluster(base_cluster_nodes, "10.10.0.0/24", "/etc/clouduler/secrets.key"), f, sort_keys=False)
    with open(joined_cluster_path, "w") as f:
        yaml.safe_dump(full_cluster, f, sort_keys=False)
    with open(joined_cluster_json_path, "w") as f:
        json.dump(full_cluster, f)

    # The internal network is widened to 127.0.0.0/8, or every node would sit
    # outside its own subnet and the config would not load at all; the cli
    # also runs on the host, where /etc/clouduler is not ours to write, hence
    # the real key path instead of the guest's.
    with open(out_control_path, "w") as f:
        yaml.safe_dump(build_cluster(control_nodes, "127.0.0.0/8", key_path), f, sort_keys=False)

    summary = {
        "name": scenario.get("name", "unnamed"),
        "description": scenario.get("description", ""),
        "mcast_group": f"230.10.{mcast_id}.1",
        "vpn_mcast_group": f"230.11.{mcast_id}.1" if vpn else "",
        "nodes": node_summaries,
        "guest_probe": scenario.get("guest_probe", ""),
        "key_file": key_path,
        "steps": scenario.get("steps", []),
    }
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
