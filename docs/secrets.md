# Configs and secrets

Both are cluster objects: they live in the replicated database, so every manager
holds them and a container can start on any node without anything having been
copied there by hand. By default, both are mounted into containers as files; a
secret can instead be exposed as an environment variable.

The difference is that a config is stored as written and a secret is encrypted
before it ever reaches the database.

## Mounting as a file

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

The short form (`- db_password`) is just the object name, using every default
below. The long form sets any of these fields explicitly:

| Field | Applies to | Default | Meaning |
|---|---|---|---|
| `source` | configs, secrets | *(required)* | Name of the cluster object to mount. |
| `target` | configs, secrets | `/run/secrets/<source>` for a secret, `/<source>` for a config | Absolute path inside the container the file is bind mounted at. |
| `mode` | configs, secrets | `0444` | Octal file permissions. A bare `0444` is read as octal, as in compose; quoting it (`"0444"`) works too. |
| `uid` | configs, secrets | unset | Owning user of the file inside the container. |
| `gid` | configs, secrets | unset | Owning group of the file inside the container. |
| `env` | secrets only | unset | Environment variable name to expose the secret's value under, instead of mounting a file. See [Exposing a secret as an environment variable](#exposing-a-secret-as-an-environment-variable). |
| `write` | configs only | `false` | Mount read-write and feed the container's own edits back into the cluster's copy. See [Writable configs](#writable-configs). |

Mounts are read only by default. A writable secret file would be a leak
waiting to happen, so `write` is rejected outright on a secret — only a
config can opt in. Each instance gets its own object directory, mode 0700,
and it is removed when the container goes away, so a secret does not outlive
the container that needed it.

## Writable configs

```yaml
services:
  app:
    image: myapp
    configs:
      - source: app_conf
        target: /etc/myapp/config.toml
        write: true
```

By default a mounted config is read only: it flows one way, from the cluster
into the container. `write: true` makes it two way. The mount itself becomes
read-write, and whenever the scheduler's periodic reconcile pass (every ten
seconds) finds the file's content no longer matches what it last wrote, it
treats that as the container's own edit and submits it back as the config's
new value — the same `PutConfig` a `clouduler config` command or the web ui
would submit, so every other instance mounting that config picks up the
change on its own next pass.

A few things follow from that:

- The default file mode changes from `0444` to `0644`, since a config nobody
  can write is not much of a writable config. An explicit `mode:` still wins.
- Only a config can be `write: true`; setting it on a secret is a validation
  error. Nothing should be able to overwrite a secret's plaintext by editing
  its own mounted copy of it.
- If the cluster's copy changes while nothing has touched the mounted file —
  another instance's edit landing, or an edit made through the web ui —
  clouduler rewrites the file to match, same as a read-only config.
- Two instances writing to the same config at nearly the same time is a plain
  last-write-wins race, same as two people editing it by hand at once. There
  is no locking.
- `env` cannot be combined with `write`, for the same reason it cannot be
  combined with `target`/`mode`/`uid`/`gid`: an environment variable is not a
  file, so there is nothing there for a container to edit back.

## Exposing a secret as an environment variable

Some images only know how to read configuration from the environment, and
otherwise need an entrypoint script that shells out to `cat` a file into a
variable before starting. Give the secret's long form an `env` key instead of
`target`/`mode`/`uid`/`gid` and clouduler sets the environment variable
directly, with no wrapper script and no file ever touching disk:

```yaml
services:
  headplane:
    image: ghcr.io/tale/headplane
    secrets:
      - source: headplane_cookie_secret
        env: HEADPLANE_SERVER__COOKIE_SECRET
      - source: headplane_api_key
        env: HEADPLANE_HEADSCALE__API_KEY
      - source: headplane_oidc_client_secret
        env: HEADPLANE_OIDC__CLIENT_SECRET
```

This replaces the earlier workaround of writing an entrypoint that does:

```yaml
    entrypoint:
      - /bin/sh
      - -c
      - |
        export HEADPLANE_SERVER__COOKIE_SECRET="$$(cat /run/secrets/headplane_cookie_secret)"
        exec /usr/local/bin/node /app/build/server/index.js
```

| Behavior | File secret (`target`) | Env secret (`env`) |
|---|---|---|
| Where the plaintext lands | A file under the node's data directory, bind mounted read only | Nowhere on disk — passed directly into the container's environment |
| Visible via `docker inspect` / `/proc/<pid>/environ` | No | Yes, like any other environment variable |
| `target`, `mode`, `uid`, `gid` | Allowed | Rejected — an env secret is not a file, so these have nothing to apply to |
| Combining with `environment:` on the same name | N/A | The secret's value wins over a same-named entry in the service's `environment:` block |
| Value encoding | Raw bytes | Must be valid UTF-8; a binary secret given an `env` target fails to deploy |

## Choosing between the two

| | File (default) | Environment variable |
|---|---|---|
| Best for | Images that read a path (`cat /run/secrets/x`, a config flag pointing at a file) | Images that only read `process.env` / `os.environ`, with no file-reading option |
| Exposure | Owner-only file mode, read only, scoped to the one instance | Whole environment, same as `environment:` |
| Setup | None — matches docker's own convention | None — no entrypoint wrapper needed |

## Where objects come from

Deploying a stack publishes the files its `configs:` and `secrets:` blocks point
at as cluster objects, owned by that stack. Removing the stack removes them,
unless another stack still references them.

They can also be created directly from the web ui, which is the only way to make
one that no stack file carries. The ui offers three ways to make a secret: paste
a string, upload or edit a file, or generate a random value of a chosen length
and character set.

Deploying or redeploying a stack from the web ui shows a field for every
`file:`-backed name its `configs:`/`secrets:` blocks declare, prefilled with the
current content where one already exists. Leaving a field blank on a redeploy
keeps the existing value rather than clearing it; only a name with no existing
value and a blank field blocks the deploy. A stack's detail page also shows a
placement table: one row per network, drive, config and secret its services
need, one column per node, so a service that cannot schedule shows exactly
which node is missing what — including a button to create a missing config or
secret inline.

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
