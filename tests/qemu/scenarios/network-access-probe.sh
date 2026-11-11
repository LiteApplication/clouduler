#!/bin/sh
# Runs in every guest of the network-access scenario (installed by mkdisk.sh
# as /etc/clouduler/probe.sh, started by guest-init.sh).
#
# An exec listener on :2222 (see container-network-probe.sh): the scenario
# runs the connection attempts from inside the guests through it, with an
# explicit source address (`nc -s`) so the peer's firewall sees a lan or dmz
# client. Plus a plain listener on :9000 answering "pong", a service on every
# address of the node (cluster, lan, dmz) for those attempts to reach.
(
  while true; do
    nc -l -p 2222 -e /bin/sh >/dev/null 2>&1
  done
) &
(
  while true; do
    echo pong | nc -l -p 9000 >/dev/null 2>&1
  done
) &
wait
