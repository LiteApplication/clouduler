#!/bin/sh
# Runs in every guest of the container-network scenario (installed by mkdisk.sh
# as /etc/clouduler/probe.sh, started by guest-init.sh).
#
# 1. An exec listener on :2222, which qemu forwards to the host's `expose_port`
#    for the node (lib/guest_exec.py talks to it). Everything this scenario
#    asserts about routes, rules and chains is read off the guest this way,
#    because none of it is visible through the api. A test vm only.
# 2. A canary on :8080 that answers with the address it saw the client come
#    from, so a container behind the router can report what the outside saw.
(
  while true; do
    nc -l -p 2222 -e /bin/sh >/dev/null 2>&1
  done
) &

cat > /tmp/canary.sh <<'CANARY'
#!/bin/sh
# Read the request first: closing with it unread would reset the connection.
while read -r line; do [ "$line" = "$(printf '\r')" ] && break; done
# busybox nc does not always export the peer address, the socket table has it.
peer=$(netstat -tn 2>/dev/null | awk '$4 ~ /:8080$/ && $6 == "ESTABLISHED" {sub(/:[0-9]+$/, "", $5); sub(/^::ffff:/, "", $5); print $5; exit}')
printf 'HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nsrc=%s\n' "$peer"
CANARY
chmod +x /tmp/canary.sh
(
  while true; do
    nc -l -p 8080 -e /tmp/canary.sh >/dev/null 2>&1
  done
) &
wait
