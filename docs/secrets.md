# Configs and secrets

Both are cluster objects: they live in the replicated database, so every manager
holds them and a container can start on any node without anything having been
copied there by hand. Both are mounted into containers as files.

The difference is that a config is stored as written and a secret is encrypted
before it ever reaches the database.

## Mounting

Docker's own configs and secrets are a swarm feature, so clouduler does the same
job itself: the node writes each object to a file under its data directory and
bind mounts it read only at the target path.

A secret lands at `/run/secrets/<name>` by default, exactly where docker puts
one, so an image that already reads docker secrets needs no changes.

```yaml
services:
  db:
    image: postgres
    configs:
      - source: pg_conf
        target: /etc/postgresql/postgresql.conf
        mode: 0444
    secrets:
      - db_password                    # -> /run/secrets/db_password
      - source: tls_key
        target: /etc/ssl/private/tls.key
        mode: 0400
```

The short form is just the object name. The long form sets `target`, `mode`,
`uid` and `gid`. A bare `0444` is read as octal, as in compose.

Mounts are always read only. A container has no business editing the cluster's
copy of a config, and a writable secret file is a leak waiting to happen. Each
instance gets its own object directory, mode 0700, and it is removed when the
container goes away, so a secret does not outlive the container that needed it.

## Where objects come from

Deploying a stack publishes the files its `configs:` and `secrets:` blocks point
at as cluster objects, owned by that stack. Removing the stack removes them,
unless another stack still references them.

They can also be created directly from the web ui, which is the only way to make
one that no stack file carries. The ui offers three ways to make a secret: paste
a string, upload or edit a file, or generate a random value of a chosen length
and character set.

## Encryption

Secrets are sealed with XChaCha20-Poly1305 before entering the database. The
database, its snapshots and any backup of them therefore carry only ciphertext.

The secret's name is bound into the encryption as associated data. That is not
decoration: without it, anyone able to edit a stack could point a service's
`db_password` at the ciphertext of some other secret and have the cluster
decrypt it into their container. Renaming a secret's ciphertext makes it
undecryptable, which is the correct outcome.

Each seal uses a fresh random nonce, so two identical secrets do not produce
identical ciphertext.

## The cluster key

The key is not in the database. It sits in a file, `secrets.key_file`, defaulting
to `/etc/clouduler/secrets.key`, holding the key base64 encoded on one line.

```yaml
secrets:
  key_file: /etc/clouduler/secrets.key
  generate_if_missing: true
```

On first launch a node generates one and logs its fingerprint with a warning to
back the file up. **If every copy of that file is lost, every stored secret is
unrecoverable.** There is no escrow and no recovery path, by design.

A node without the key is a valid setup, not an error. It replicates sealed
secrets like any other state and simply cannot run a container that needs one;
the reconcile loop refuses to start it rather than mounting an empty or
encrypted file. Set `generate_if_missing: false` on such a node so a typo in the
path cannot quietly create a second, useless key.

The fingerprint is a truncated hash of the key. It is safe to log and to show in
the ui, and lets you check two nodes hold the same key without either revealing
it. A secret records the fingerprint it was sealed under, so a node holding the
wrong key reports exactly that instead of a decryption failure.

The key file is created mode 0600 and clouduler refuses to read one that other
users can see.

## After an outage

When rebuilding a cluster, the key has to get back onto a node before any
container needing a secret can start. Either restore the file from a backup, or
upload it through the web ui, which writes it to `secrets.key_file` on that node.
Uploading is refused if a key is already present, since replacing it would
orphan every stored secret.

## Deploying from the CLI

`clouduler stack deploy` seals secrets locally, because plaintext must never
travel through the raft log. That means the machine running the command needs
the key. Deploying a stack with secrets from a machine without one fails with a
message saying so; deploy from a node that holds the key, or use the web ui.

A stack with only configs and no secrets needs no key at all.
