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
    drives_block = {
        name: {"sync_interval": drives_cfg.get(name, {}).get("sync_interval", "30s")}
        for name in sorted(all_drive_names)
    }

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

    node_summaries = []
    cluster_nodes = {}
    for index, n in enumerate(nodes):
        name = n["name"]
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
        if vpn and name == vpn["egress_node"]:
            # A bare non-IP string is the third `NodeAddress` form: a host
            # interface name, naming eth2 (the vpn-segment netdev) rather
            # than an address on it.
            node_network[vpn["network"]] = "eth2"
        cluster_nodes[name] = {
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

    cluster = {
        "cluster": {"name": scenario.get("name", "qemu-test")},
        "networks": {
            "cluster": {"type": "internal", "subnet": "10.10.0.0/24", "domain": "cl.test"},
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
        },
        "drives": drives_block,
        "nodes": cluster_nodes,
        "web": {"address": "api", "network": "cluster", "port": 8443},
        "failover": failover,
        "secrets": {"key_file": "/etc/clouduler/secrets.key", "generate_if_missing": False},
    }

    with open(out_cluster_path, "w") as f:
        yaml.safe_dump(cluster, f, sort_keys=False)

    # control.yaml: same shape, but every node address points at its
    # host-forwarded loopback address and port, since the CLI runs on the host
    # and has no route onto the VMs' own cluster network. The internal network
    # is widened to 127.0.0.0/8 to match, or every node would sit outside its
    # own subnet and the config would not load at all.
    control_nodes = {}
    for ns in node_summaries:
        control_network = {"cluster": ns["control_ip"]}
        if vpn and ns["name"] == vpn["egress_node"]:
            control_network[vpn["network"]] = "eth2"
        control_nodes[ns["name"]] = {
            "role": ns["role"],
            "network": control_network,
            "rpc_port": ns["rpc_port"],
            "drives": {d: f"/srv/clouduler/{d}" for d in ns["drives"]},
        }
    control = dict(cluster)
    control["networks"] = {
        "cluster": {"type": "internal", "subnet": "127.0.0.0/8", "domain": "cl.test"},
        "wan": {"type": "public", "domain": "wan.test"},
        **egress_networks_block,
    }
    control["nodes"] = control_nodes
    # The cli runs on the host, where /etc/clouduler is not ours to write.
    control["secrets"] = {"key_file": key_path, "generate_if_missing": False}
    with open(out_control_path, "w") as f:
        yaml.safe_dump(control, f, sort_keys=False)

    summary = {
        "name": scenario.get("name", "unnamed"),
        "description": scenario.get("description", ""),
        "mcast_group": f"230.10.{mcast_id}.1",
        "vpn_mcast_group": f"230.11.{mcast_id}.1" if vpn else "",
        "nodes": node_summaries,
        "key_file": key_path,
        "steps": scenario.get("steps", []),
    }
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
