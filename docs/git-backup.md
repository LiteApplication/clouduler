# Git-backed backup and restore

Clouduler can continuously mirror its replicated content - the cluster config,
stack specs, cluster configs and secrets - into a git repository you control.
It exists for the case every node's raft data is lost for good: with nothing
else, that content is gone with it. This is content recovery, not membership
recovery; see [Disaster recovery](#disaster-recovery-and-emergency-recovery)
below for how it relates to `clouduler emergency`.

## What gets exported

Every export is a full rebuild of the working tree, not an incremental patch:

```
cluster.yaml
stacks/<name>.yaml
configs/<name>
secrets/<name>.age
```

`cluster.yaml` and each stack spec are exported as their raw source, byte for
byte. Each cluster config is the decoded content of the object. Each secret is
decrypted with the cluster's own key and re-encrypted for the age recipient
you supply - the cluster's own key is never what protects the copy in git.

**Not exported:** which stack owns a bundled config or secret. A restore
brings every config and secret back, just without that ownership link. Also
not exported: node roster history, running-instance placement, DNS/certificate
state, and anything else outside the four kinds listed above - all of that is
either already in `cluster.yaml` or is reconstructed by the scheduler once the
cluster is running again.

## Setup

1. Generate an age key pair for yourself (`age-keygen`), somewhere outside the
   cluster entirely. Clouduler never sees the private half.
2. Create an empty git repository the cluster can push to.
3. From the web ui's Configuration > Git backup page, turn the feature on and
   fill in the repo's SSH URL, your age public key (`age1...`), the branch to
   push, and how often to check for changes.
4. Save. The leader generates an SSH deploy keypair the first time it exports
   and shows the public half on the same page - add it to the repo as a
   write-capable deploy key (on GitHub: repo Settings > Deploy keys, with
   write access; other hosts have an equivalent). Nothing is pushed
   successfully until this step is done.

```yaml
backup:
  repo_url: git@github.com:you/clouduler-backup.git
  age_recipient: age1qqf8s8u...
  branch: main
  interval: 30s
```

## How the deploy key works

The keypair is generated once, cluster-wide - not per node - because
leadership can fail over between managers, and whichever node currently leads
is the one that pushes. A per-node key would need every manager registered on
the git host individually. The public half is stored as an ordinary,
plaintext cluster config object; the private half is stored as a cluster
secret, sealed the same way any other secret is. Either can be inspected
through the normal `/api/configs`/`/api/secrets` machinery only in the sense
that they are excluded from it by name - they never appear in the generic
listing, the same way the self-update deploy key does not.

## Commit behavior

The leader checks for changes every `interval`. A tick with nothing new to
export is a no-op: no empty commits. When there is something to export, the
commit message describes what changed - which stack, config, or secret was
added, updated, or removed, or that the cluster config itself changed - so
the repo's history reads as a log of cluster changes, not "clouduler backup"
repeated forever.

The last export attempt (timestamp, summary, commit hash if it succeeded, or
the error if it did not) is shown on the Git backup config page. A failed
export is retried on the next tick; it does not block anything else from
running.

## Restoring

`clouduler restore` rebuilds one node's local content from a git backup. It
runs offline, the same way `clouduler emergency` does - stop the node's
daemon first, since both write straight to its local `raft.redb`.

```
clouduler restore <source> --age-identity-file ~/my-age-key.txt [--branch main] [--node NAME]
```

`<source>` is normally a local path to an already-cloned copy of the backup
repo - clone it yourself first, with whatever SSH credentials you have.
(A `git@`/`ssh://` URL is also accepted and cloned directly using your ambient
SSH agent, but a fresh restore cannot rely on the cluster's *own* deploy key
being available, since that key is itself part of the state being restored.)
The age identity is read once, used in memory to decrypt and immediately
reseal each secret under this node's own cluster key, and never written
anywhere. A secret that cannot be decrypted with the identity given is
skipped and reported, not fatal - the rest of the restore still proceeds.

Every restored object gets a fresh revision number; the numbers a commit
history carries are meaningless to a freshly rebuilt state machine. The
command prints exactly what it is about to replace and asks for confirmation
unless `--yes` is passed.

## Disaster recovery and emergency recovery

`restore` never touches raft membership or the log - only the four kinds of
content above. If a cluster has *also* lost quorum for good (every manager's
raft state gone, not just its content), a normal membership change cannot be
driven through raft either, since that itself needs the quorum that is
missing. Recovering from that needs `clouduler emergency` first, which forces
one node to become the cluster's sole voter, bypassing consensus the way
etcd's `--force-new-cluster` does. See its own `--help` for what it changes.

A full disaster recovery is therefore both, in order:

```
clouduler emergency <stack> --yes   # first: recover a reachable quorum
clouduler restore <source> ...      # then: refill it with backed-up content
```

They compose rather than overlap: `emergency` says nothing about the cluster's
*content*, and `restore` says nothing about its *membership*.

## Limitations

- SSH only. An `http(s)://` repo URL is rejected at config validation time.
- One-way export. Nothing is ever read back from the repo except by
  `clouduler restore`, and only when you run it deliberately.
- Config/secret ownership by a specific stack is not preserved across a
  restore (see [What gets exported](#what-gets-exported)).
- The SSH-credential push path itself is not covered by the automated test
  suite (only the git plumbing around it is) - smoke test against a
  throwaway repo before relying on this for a real cluster.
