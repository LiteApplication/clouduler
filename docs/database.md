# The cluster database

Every node keeps a full copy of the cluster database in `<data_dir>/raft/raft.redb`.
Managers vote on changes; workers hold a read-only replica so they can reconcile
their own containers without asking anyone.

## What is in it

- The node roster: roles, addresses, implemented drives, availability, and which
  nodes the leader currently cannot reach.
- Stacks: the yaml exactly as submitted, a revision counter, and the contents of
  every file the stack references.
- Placements: which instance runs on which node, its desired state, its pinned
  image digest, and any migration in progress.
- Volumes: the generation counter and which node has caught up to which
  generation.
- Certificates and the DNS records clouduler owns.
- Overlay address allocations.

## How a change gets everywhere

A write is submitted to any node. If that node is not the leader it forwards the
request over the same RPC channel raft uses, so callers never need to know who
leads. The leader appends the command to the log, replicates it, and once a
majority of managers have it on disk the command is applied to every node's state
machine in the same order. Applying is deterministic and does no IO, which is
what makes every copy identical.

Without a quorum, no write is accepted at all. Reads are served locally from the
applied state, so they can trail the leader by the replication delay.

Rejected commands still occupy a log entry. A command that violates an invariant,
say placing an instance on a node that does not implement its drive, is applied
everywhere as a rejection with a reason, and the state version does not move.

## Durability

The state machine is written out in full on every apply rather than being
rebuilt from the log at startup. The whole state is a few kilobytes at this
scale, so the cost is not worth optimising away, and recovery becomes a read
rather than a replay. Snapshots are a json dump of the same structure.

## Stack files

`configs` and `secrets` in a stack file point at paths on whichever machine ran
the deploy. Those paths mean nothing on the node that ends up running the
container, so `clouduler stack deploy` reads them once and stores the contents
alongside the spec:

```
clouduler stack deploy ./site.yaml
clouduler stack list
clouduler stack show site
clouduler stack show site --files
clouduler stack remove site
```

Every manager then holds the stack and its files, and any node implementing the
right drives can run the containers.

The files a stack carries are capped at 1 MiB in total. Everything here travels
through the raft log and into every snapshot, so a stack is not the place to put
a disk image; use a volume on a drive for that.

### Secrets

An attachment is flagged as a secret when it came from the `secrets:` block.
Secret contents are stored in the database like anything else, but they are
blanked out of any state that leaves a node, and the `Debug` output of an
attachment never renders them, so they cannot end up in a log line by accident.
`clouduler stack show --files` lists secret names and never their contents.

The `include_secrets` flag on a state query exists for a restore path. The cli
never sets it.
