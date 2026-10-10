# MCP server

Every node serves a [Model Context Protocol](https://modelcontextprotocol.io)
endpoint at `https://<web fqdn>/mcp` (Streamable HTTP), so an AI client such as
Claude Code, claude.ai, Cursor or VS Code can inspect the cluster, build and fix
stacks, and debug inside containers. It is part of the API server, so there is
nothing extra to deploy.

## Turning it on and off

```yaml
mcp:
  enabled: true
```

`clouduler init` writes `enabled: false` into a new cluster's config, so
letting an AI client in is an explicit opt-in. A config with no `mcp:` key at
all counts as enabled, so clusters created before the setting existed keep
the access they already had.

The setting is read live (and can be flipped from Configuration → AI access in the ui), so it takes effect without a restart. When it is off,
`/mcp`, the OAuth discovery documents, `/oauth/*` and `/api/oauth/*` all answer
404, and the "AI access" page disappears from the ui. `/api/mcp/clients` keeps
working so an admin can still see and revoke previously connected clients.

## Connecting

The **AI access** page in the ui shows the endpoint and one-click install links:

- Claude Code: `claude mcp add --transport http clouduler https://<fqdn>/mcp`
- claude.ai: add a custom connector with the same URL under
  Settings → Connectors
- Cursor and VS Code: install deep links

The client discovers the OAuth server on its own (RFC 9728 protected-resource
metadata, RFC 8414 server metadata, RFC 7591 dynamic registration). It opens
the clouduler consent page in your browser. You log in if you are not already,
choose **read-only** or **admin**, and approve. Only admins can grant admin.

Grant details:

- **Access tokens:** valid 1 hour, signed with a key derived from the cluster
  key, so every node can verify them.
- **Refresh tokens:** valid 30 days and rotated on every use.
- **Revoking:** revoke a client from the AI access page. This takes effect on
  the client's next request, even mid-session, because every token check
  confirms the client record still exists.
- **Role changes:** an admin client keeps admin until it is revoked, even if
  the user who approved it is later demoted.
- **API keys:** `Authorization: Bearer clk_…` also works on `/mcp` for headless
  clients, always as an admin grant.

Client records and refresh-token hashes are replicated through raft under the
reserved config `_clouduler_mcp_clients`. It is hidden from `/api/configs` and
left out of the git-backup export, because every refresh would otherwise be a
commit. After restoring a cluster from backup, clients have to be approved
again.

MCP protocol sessions, stack focus, pending consent requests and authorization
codes live in memory on the leader. Followers relay `/mcp` to it. A leader
change drops them, and clients start a new session (and you re-focus the stack).

## Focus

The AI picks the one stack it is working on with `focus_stack`. Focus belongs to
the MCP session and the identity that set it. It does two things:

- **Reports narrow to that stack.** `list_problems`, `cluster_overview` and
  `list_instances` describe it in full, and give only a count for problems
  elsewhere. Cluster-wide faults (quorum, certificates, backups) are always
  shown. Nothing about the cluster's own health checking or rescheduling
  changes.
- **Writes are limited to it.** Deploys, removal, restarts, config and secret
  writes, `exec` and `read_file` refuse anything outside the focused stack, and
  are refused entirely with no focus.

## Tools

| Tool | Grant | What it does |
|------|-------|--------------|
| `focus_stack`, `get_focus`, `clear_focus` | read | set, show or clear the working stack |
| `cluster_overview` | read | leader, quorum, nodes, counts, cluster-wide faults |
| `list_problems` | read | unhealthy or unplaceable instances, crash loops, failed updates, proxy, backup, cert and DNS errors |
| `list_stacks`, `get_stack` | read | stacks, spec YAML, requirements, instances |
| `list_instances`, `get_instance` | read | instance state, health, node, placement error, proxy health |
| `get_logs` | read | container log tail for an instance or every replica of a service, with optional `grep` |
| `list_nodes`, `get_cluster_config` | read | what a stack may use: networks, drives, domains, ports |
| `list_configs`, `get_config` | read | cluster configs |
| `list_secrets`, `inspect_secret` | read | secret names and facts, never values |
| `validate_stack` | read | validate a spec without deploying |
| `deploy_stack` | admin | deploy the focused stack, then wait for and report instance states |
| `remove_stack` | admin | remove the focused stack (`confirm` must repeat its name) |
| `restart_instance`, `update_instance` | admin | restart, or pull and recreate |
| `put_config` | admin | create or update a config |
| `generate_secret` | admin | generate and store a secret the AI never sees |
| `exec` | admin | run a command in a focused-stack container (no tty, 300 s max, 64 KiB per stream) |
| `read_file` | admin | read a file or list a directory in a focused-stack container (works in distroless images) |

A read-only grant does not see admin tools in `tools/list`, and calling one
anyway is refused.

Resources: `clouduler://docs/{stacks,networking,secrets,proxy}` and
`clouduler://examples/*.yaml`, compiled into the binary.

## Secrets

The AI can work with secrets without ever holding one.

- **`generate_secret`** takes a name, length, character classes (lower, upper,
  digits), symbols (`true` for a safe default set, or an explicit string such as
  `"-_"`), `exclude_ambiguous` and `require_each_class`. The value is generated
  and sealed on the server, and only its facts come back. Existing secrets are
  kept unless `overwrite` is set, because rotating a password a running database
  already uses breaks it. Reserved (clouduler-internal) names are refused.
- **`inspect_secret`** reports:
  - existence, revision and owner
  - which services use the secret, and as which env var or file path
  - byte and character length, and whether it is valid UTF-8
  - leading or trailing whitespace, a trailing newline, internal whitespace or
    newlines
  - which character classes are present

  It never returns a hash, prefix or suffix: a hash of a short secret can be
  brute-forced.
- **Deploys** can't carry secret plaintext. `deploy_stack` has no secrets
  parameter. `deploy_stack` and `put_config` also refuse content that already
  contains a stored secret's value.

### Redaction

Every tool result passes through one function, which:

1. decrypts **every** cluster secret, including clouduler's own;
2. replaces each one with `[REDACTED secret:<name>]`, catching the value raw,
   trimmed, line by line, as base64 (standard and url-safe, at any alignment
   inside a larger blob, so `base64("user:" + secret)` in a basic-auth header
   is caught), as hex, percent-encoded or JSON-escaped;
3. then truncates the result to 256 KiB. Truncating after redacting means a
   secret can't survive by being split at the cap.

The redactor is rebuilt for every call, so a secret rotated a second ago is
already covered.

Things to keep in mind:

- **No cluster key, no output.** A node without the cluster key cannot redact,
  so it refuses every tool except `get_focus` and `clear_focus`.
- **Short values aren't redacted.** Values under 4 characters would match all
  over ordinary text, so they are skipped and named in a note under the result.
  The same note lists any secret this node could not decrypt.
- **Multi-line secrets are matched line by line.** For a secret that is a
  whole file, an ordinary line in it can be redacted wherever it shows up.
- **`read_file` refuses secret paths.** It won't read `/run/secrets` or a
  service's own secret mount targets, after normalizing the path.
- **It is not a sandbox.** `exec` runs arbitrary commands in a container that
  holds its secrets. Redaction stops accidental and casual exposure: `cat`,
  `printenv`, `base64`, logs echoing a connection string. An AI that
  deliberately transforms a value (reverses it, splits it, encrypts it) can get
  it out. Only grant admin to clients you would trust with a shell in your
  containers.

Every tool call is logged at info level with the subject, client, tool and
target, after redaction.
