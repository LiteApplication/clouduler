# Git-backed backup and restore

Clouduler can continuously mirror its replicated content - the cluster config,
stack specs, cluster configs and secrets - into a git repository you control.
It exists for the case every node's raft data is lost for good: with nothing
else, that content is gone with it. This is content recovery, not membership
recovery; see [Disaster recovery](#disaster-recovery-and-emergency-recovery)
below for how it relates to `clouduler emergency`.

## What gets exported

Every export is a full rebuild of the tree, not an incremental patch:

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

Any other files or directories present in the tracked branch (e.g. a README,
CI config, or .gitignore an admin added by hand) are never exported - every
commit built by the export process contains exactly and only the four tracked
kinds listed above. Admin-added files are not clobbered on disk (there is no
working directory) but they are not carried forward into new exports either;
the tree is always rebuilt from scratch against the remote's current tip.

## How git storage works internally

Both export and pull operate against a **bare git repository** on the leader
node - a directory containing only the bare git object database (no working
tree, no `HEAD` checkout). Commits and trees are constructed entirely via
git2's object-database APIs (`Repository::blob`, `TreeBuilder::insert`,
`Repository::commit`) without ever writing files to a working directory. This
means there is no checked-out working copy to conflict, no file-system
locking to worry about, and no stale state between ticks.

Exports build the full tree from raft state, compare it to the remote's
current tip (fetched fresh each tick), and only commit+push when something
changed. Pulls fetch the remote tip fresh, build the cluster's *current live
state* into a virtual tree (the same tree a fresh export would produce), and
compare the two. This comparison is always against what the cluster is
**running right now**, not against a historical bookmark, so leadership
rotation between an export and a pull cannot cause a spurious rejection.

## Setup

1. Generate an age key pair for yourself (`age-keygen`), somewhere outside the
   cluster entirely. Clouduler never sees the private half.
2. Create an empty git repository the cluster can push to.
3. From the web ui's Configuration > Git backup page, turn the feature on and
   fill in the repo's SSH URL, your age public key (`age1...`), and the branch
   to push.
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

The leader watches for changes and pushes as soon as it sees one, on a short
poll (a couple of seconds) rather than a fixed interval. A poll with nothing
new to export is a no-op: no empty commits. When there is something to
export, the commit message describes what changed - which stack, config, or
secret was added, updated, or removed, or that the cluster config itself
changed - so the repo's history reads as a log of cluster changes, not
"clouduler backup" repeated forever. The git2/age work itself runs off the
main event loop, so a slow push never delays anything else the leader is
doing.

Each export fetches the remote's current tip first, builds the candidate
tree from raft state, and compares its tree OID to the remote tip's tree
OID. If they are identical the export is a no-op (no commit created, no
push attempted). If different, a new commit is created whose parent is the
freshly-fetched remote tip (or no parent if the branch does not yet exist on
the remote), and pushed non-force. If the push is rejected because the remote
moved concurrently, the export retries once: it re-fetches the tip, rebuilds
the commit against the new parent, and pushes again. Since no local branch
state is persisted, there is no divergence to reconcile.

The last export attempt (timestamp, summary, commit hash if it succeeded, or
the error if it did not) is shown on the Git backup config page. A failed
export is retried on the next poll; it does not block anything else from
running.

## Pulling changes back (git → cluster)

A push to the tracked branch can flow back into the cluster too, the reverse
of the export above. On the Git backup config page, set a webhook token, then
point the repository's webhook at:

```
POST https://<any-node>/api/git-backup/webhook/<token>
```

The token lives in the url path rather than a header, since most webhook
integrations (and some git hosts) offer no way to attach a custom header but
all of them accept an arbitrary url. Treat the whole url as the secret, the
same as you would a header token - anyone who has it can trigger a pull.

Any node can receive the request; a non-leader forwards it internally to
whichever node currently leads, since only the leader has the deploy key.
The leader fetches the tracked branch and:

1. Compares the remote candidate commit with the cluster's last known good
   commit recorded in Raft (from previous exports, pulls, or reconciles). If the
   candidate matches the last known good commit or tree, the pull is a no-op.
2. Compares the `secrets/` subtree OID between the last known good commit and
   the incoming candidate commit. If the candidate touches any secrets, the pull
   is rejected - secret rotation must go through the API, not via git. When
   rejected, the candidate commit history is preserved on `rejected/<branch>-<sha>`
   with an explanatory commit on top, and the tracked branch is force-pushed back
   to the last working commit (the state right before the bad pull happened).
3. If secrets were not modified, reads the candidate's `cluster.yaml`, stack
   specs, and plain configs directly from the fetched tree objects (no
   working-directory checkout) and validates them.
4. On success, applies the pulled tree: reconciles cluster state by
   diffing the parsed tree against the current cluster state and applying
   the delta (adding, updating, or removing configs, stacks, and secrets).
   The accepted commit is recorded in Raft as the new last known good commit.

The last pull's outcome (applied commit, or the rejection reason and
preserved branch) shows on the same config page.

## Commands

```
clouduler git-backup pull         # run the pull manually, e.g. to test the setup
clouduler git-backup reconcile    # force cluster state to mirror into the git repo
```

`pull` is exactly what the webhook triggers, without needing a working webhook
to test it. It talks to the cluster over the same internal, key-authenticated
channel `clouduler logs`/`exec` use, not the webhook token.

`reconcile` clones/fetches the repository, removes `cluster.yaml`, `stacks/`,
`secrets/`, and `configs/` while preserving unmanaged files in the repo (such as
`README.md`, `AGENTS.md`, or docs), regenerates all cluster objects fresh from
Raft data, commits and pushes to the git repository, and updates Raft with the
new commit as the last known good commit. webhook token.

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

Restore reads the backup content from a real cloned repository on disk (using
the standard `parse_dir` path that reads checked-out files). This is a
one-shot offline admin operation where the clone is a local filesystem path,
so there is no need for the virtual tree approach used by the hot-path export
and pull code.

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
- Config/secret ownership by a specific stack is not preserved across a
  restore (see [What gets exported](#what-gets-exported)).
- The SSH-credential push path itself is not covered by the automated test
  suite (only the git plumbing around it is) - smoke test against a
  throwaway repo before relying on this for a real cluster.
