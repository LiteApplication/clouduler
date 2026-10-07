# Setting a node up

`clouduler init` is what turns a machine into a node. It writes the cluster
config file, joins or forms a cluster, creates the directories drives are backed
by, puts the cluster key in place and installs the systemd unit.

Nothing it writes is final. Every field it sets is also editable from the web ui
afterwards, and an edit there comes back down to the config file on every node
(see [Config replication](#config-replication) below), so init is a starting
point rather than the only way in.

## The guided walk

```
sudo clouduler init
```

It asks about each step and prints the value it would use in brackets. Pressing
enter takes that value, which is how a step is skipped:

```
node name [n1]:
== cluster
is this the first node?
 * 1) create a new cluster with this node as its first manager
   2) join a cluster that already exists
choice [create a new cluster with this node as its first manager]:
```

Nothing is written until every question has been answered, so a `ctrl-c`
halfway through leaves the machine as it was.

## The scripted form

`--yes` stops it asking anything: every question takes its default and a
question with no sensible default is an error rather than a hang. That is the
form for a provisioning script.

Adding a node that already has an address on the internal network:

```
clouduler init --yes \
  --join 10.10.0.1 \
  --token-file /run/clouduler.token \
  --role worker
```

That is the whole command. The node name comes from the hostname and the
internal address from the routing table: init asks the kernel which source
address it would use to reach the cluster's internal subnet, and uses that. Pass
`--ip` when the machine sits on several networks and the wrong one is picked.

Forming a new cluster without any questions:

```
clouduler init --yes \
  --cluster-name home \
  --subnet 10.10.0.0/24 \
  --internal-domain int.example.com \
  --web-address cloud \
  --drive fast=/srv/clouduler/fast
```

Useful flags:

| flag | |
|---|---|
| `--node NAME` | node name, default the hostname |
| `--role manager\|worker` | manager votes and can lead, worker only runs containers |
| `--ip ADDR` | internal address, default detected from the routing table |
| `--drive NAME=PATH` | a drive this node implements, repeatable |
| `--node-data-dir PATH` | where this node keeps its database, default the cluster's |
| `--token`, `--token-file` | api key for the node being joined through |
| `--tls-fingerprint` | certificate to expect from that node |
| `--force` | overwrite an existing config, or replace an existing node entry |
| `--no-systemd`, `--no-start` | skip the unit, or install it without starting |

## Joining

The joining machine is not in raft yet, so it cannot read the replicated config.
It fetches it over the http api instead:

1. `GET /api/config` from the node given to `--join`.
2. Add itself locally: name, role, address, drives.
3. `PUT /api/config` followed by `POST /api/config/confirm`.
4. Write the result to its own config file and start the daemon.

The confirm matters. A config edit rolls itself back if nobody confirms it,
which for a join would mean the node quietly disappearing from the cluster a
minute after it appeared.

The api key comes from the cluster: one is minted and logged on the first launch
of a fresh cluster, and more can be made from the ui. It has to be an admin key,
since the join is a config edit.

### The certificate

A cluster that has not yet been issued a real certificate serves a self signed
one, so there is no chain to check. What can be checked is that the certificate
is the one you expected:

```
clouduler init --join 10.10.0.1 --tls-fingerprint 8f:2a:...:c1
```

Without `--tls-fingerprint` the certificate is accepted and its fingerprint
printed, for you to compare against the node you are joining and to pass to the
next node you add. There is no third option that is both automatic and safe.

### The cluster key

Secrets are sealed with a key held in a file, not in the database. A joining
node **must not** create its own: the cluster's secrets are already sealed with
the one that exists, and a second key produces nothing but a node that thinks it
can open them.

So init writes `generate_secret_key: false` into the joining node's entry, which
overrides the cluster wide `secrets.generate_if_missing`, and then installs the
real key.

The key also authenticates the node to node channel (raft, forwarded writes and
the cli): every connection proves it holds the cluster key before a single
request is read, so a node without it is refused by every peer and could never
be told the key through raft. It therefore has to be on disk before the joining
node's daemon first starts, and init takes care of that over the same https
connection and admin api key it already used to join. It generates this node's
own keypair (`secrets.node_key_file`), sends the public half to
`POST /api/secrets/key/wrap`, and the cluster answers with the key wrapped for
that public key. Only this node's private key opens it, and the plaintext key
never crosses the wire. With `--yes` this is what happens. Interactively you can
instead point at a copy of the key you already have, or skip and put the file in
place yourself; `--key-file PATH` does the same without the prompt. If the fetch
fails (the node you joined through holds no key, say) init says so and carries
on, and you place the file before starting the daemon.

## The systemd unit

Written to `/etc/systemd/system/clouduler.service`:

```ini
[Service]
ExecStart=/usr/local/bin/clouduler --config /etc/clouduler/cluster.yaml daemon --node n1
Restart=always
```

Docker is `Wants=`, not `Requires=`. A manager whose docker is broken still
votes, holds the replicated state and serves the ui, and it is the node you go
to in order to find out why docker is broken, so the daemon has to come up
without it.

## Config replication

The cluster config file is replicated through raft, as one entry holding the
file's text and a generation number.

- The first leader of a new cluster publishes its own file, once. After that the
  stored config is the authority, so a node coming back with a stale file cannot
  push it over everyone else's.
- Editing through `PUT /api/config` goes through raft first. Without a quorum
  the edit lands nowhere, rather than on one node's disk only.
- Every daemon watches the stored entry and writes its own config file when the
  generation moves, through a temp file and a rename so a reader never sees half
  a config.
- Edits carry the generation they were made against, so two admins saving from
  stale views get a conflict instead of one silently overwriting the other.

Running subsystems are not reconfigured in place: the file is ahead of the
process until the daemon restarts. Rebuilding the overlay or raft membership
from a config diff under a live cluster is not something to do behind the
operator's back, so it is not done at all.

## Verifying

```
clouduler check                 # parse the config and print what it means
clouduler status                # live overview: what is wrong right now
systemctl status clouduler
journalctl -u clouduler -f
```

`clouduler status` is meant to be the one command to run when something looks
off. It asks a manager (`--via <node>` to pick which) and prints, in order:

- the raft leader, whether quorum holds, voters reachable, and emergency mode
  if it is active;
- every node: role, reachable or not, availability and instance count;
- every unhealthy instance with its node, uptime, the reason it is unhealthy
  (crashed, restart-looping, stuck mid-migration, unplaceable, running an
  image other than the pinned one, stopped) and the last 10 lines of its log;
- unreachable nodes and volumes whose primary is gone or whose replicas lag;
- image updates that failed or have made no progress for 15 minutes;
- the latest failed or warning backup per target, and failed or pending
  restores;
- certificates and DNS records that are failing or still pending.

Sections with nothing to report are omitted, and a healthy cluster ends with
`all clear`. `clouduler stack unhealthy` is the instance part of this on its
own, with flags to choose the detail.

The first launch of a fresh cluster logs the generated api key and the cluster
key fingerprint. Both are logged once and never again.
