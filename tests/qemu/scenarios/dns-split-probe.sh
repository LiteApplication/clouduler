#!/bin/sh
# Runs inside each guest of the dns-split scenario (installed by mkdisk.sh as
# /etc/clouduler/probe.sh, started by guest-init.sh). Asks the client dns
# server of another node for each hostname, once over every address range this
# node shares with it (the next node, or on the lan the other lan node). Which network the server sees us as is decided by
# the address asked: the kernel sources a query to 192.168.50.x from our own
# 192.168.50.x, to 203.0.113.x from our 203.0.113.x, to 10.10.0.x from our
# 10.10.0.x. Every answer goes to the serial console as
#   DNSPROBE from=<node> net=<range asked over> name=<host> -> <answer>
# with <answer> the sorted addresses, REFUSED, or NONE (no usable reply).

SELF="$(hostname)"
N="${SELF#n}"
TARGET=$(( N % 3 + 1 ))

ask() {
  out=$(timeout 6 nslookup -type=A "$2" "$1" 2>&1)
  if echo "$out" | grep -q REFUSED; then echo REFUSED; return; fi
  ips=$(echo "$out" | awk '/^Name:/{f=1; next} f && /^Address/{print $NF}' | sort | tr '\n' ',')
  echo "${ips:-NONE}"
}

while true; do
  for net in cluster lan wan; do
    case $net in
      cluster) srv="10.10.0.$TARGET" ;;
      lan)
        ip -4 addr show dev eth0 | grep -q 'inet 192\.168\.50\.' || continue
        # Only n1 and n2 hold a lan address, so they ask each other.
        srv="192.168.50.$(( 3 - N ))" ;;
      wan) srv="203.0.113.$TARGET" ;;
    esac
    for name in app.wan.test intranet.lan.test admin.cl.test nope.invalid; do
      echo "DNSPROBE from=$SELF net=$net name=$name -> $(ask "$srv" "$name")"
    done
  done
  sleep 3
done
