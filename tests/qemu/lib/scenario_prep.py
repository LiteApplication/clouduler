#!/usr/bin/env python3
"""Reads a scenario yaml file, allocates host ports, writes the generated
cluster.yaml for the run, and prints a JSON summary for the bash driver to
consume with jq. Kept separate from the bash side because parsing yaml and
picking free ports is much less painful in python than in bash.
"""
import json
import socket
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

    node_summaries = []
    cluster_nodes = {}
    for n in nodes:
        name = n["name"]
        web_port = free_port()
        rpc_port = free_port()
        node_drives = n.get("drives") or {}
        cluster_nodes[name] = {
            "role": n["role"],
            "network": {"cluster": n["ip"]},
            "rpc_port": 7373,
            "drives": {d: f"/srv/clouduler/{d}" for d in node_drives},
        }
        node_summaries.append({
            "name": name,
            "role": n["role"],
            "ip": n["ip"],
            "web_port": web_port,
            "rpc_port": rpc_port,
            "drives": list(node_drives.keys()),
        })

    cluster = {
        "cluster": {"name": scenario.get("name", "qemu-test")},
        "networks": {
            "cluster": {"type": "internal", "subnet": "10.10.0.0/24", "domain": "cl.test"},
        },
        "drives": drives_block,
        "nodes": cluster_nodes,
        "web": {"address": "api", "network": "cluster", "port": 443},
        "failover": failover,
    }

    with open(out_cluster_path, "w") as f:
        yaml.safe_dump(cluster, f, sort_keys=False)

    # control.yaml: same shape, but every node address points at its
    # host-forwarded port on 127.0.0.1, since the CLI runs on the host and
    # has no route onto the VMs' own cluster network.
    control_nodes = {}
    for ns in node_summaries:
        control_nodes[ns["name"]] = {
            "role": ns["role"],
            "network": {"cluster": "127.0.0.1"},
            "rpc_port": ns["rpc_port"],
            "drives": {d: f"/srv/clouduler/{d}" for d in ns["drives"]},
        }
    control = dict(cluster)
    control["nodes"] = control_nodes
    with open(out_control_path, "w") as f:
        yaml.safe_dump(control, f, sort_keys=False)

    summary = {
        "name": scenario.get("name", "unnamed"),
        "description": scenario.get("description", ""),
        "mcast_group": f"230.10.{mcast_id}.1",
        "nodes": node_summaries,
        "steps": scenario.get("steps", []),
    }
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
