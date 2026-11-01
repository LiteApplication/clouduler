# Alerts

Clouduler raises alerts for things that need a human: a crash-looping
instance, a full disk, a failed backup, a certificate about to expire, a lost
quorum. Every alert shows in the UI (Alerts page, in-app, always) and can be
sent to **destinations**: a Discord channel, any webhook, or email.

## How it works

The raft leader runs the alert engine. Every few seconds it derives the
current problems from the replicated state (the same checks as
`list_problems`), reads the signals other nodes pushed (OOM kills, disk and
memory pressure, log matches, self-update results) and keeps one record per
alert in raft. Nothing is stored outside raft, so a new leader carries on
from the same records.

- A **warning** is held back for `warning_debounce` (default 5 minutes). If
  it clears first it is dropped without a trace. A **critical** alert is sent
  at once.
- An alert resolves after two consecutive clean readings, so one missed
  reading never flaps it. An instance whose node does not answer keeps its
  alerts instead of resolving them.
- When an open alert gets worse (warning to critical) a new message is sent
  (`renotify_on_escalation`). When an alert that was delivered resolves, a
  resolved message goes to the destinations that received the opening one
  (`notify_resolved`).
- **Info** alerts are collected into a daily digest sent at `digest_at` in
  `timezone` to destinations with `digest: true`.
- Delivery is at most once across a leader change. Each destination gets up
  to 5 attempts (10 s timeout each, backing off 10 s, 30 s, 60 s, 120 s, and
  honouring `Retry-After`). A destination receives at most 30 messages per
  10 minutes; the rest are recorded as held back.
- Without quorum nothing can be written to raft, so each manager that has
  been without quorum for more than a minute sends one critical
  `cluster.quorum_lost` message directly to the destinations it knows. When
  quorum returns, the leader sends the resolved message.
- Message text is passed through the same redaction as the MCP tools: values
  of cluster secrets never leave in a message.

Every setting below applies live; nothing needs a daemon restart. Settings
are read on each engine pass and again on each delivery attempt.

## Settings

Stored as the reserved config `_clouduler_alerts` (JSON), edited on the
Alerts page or through `PUT /api/alerts/settings`. It is not exported to git
backup and not shown in the configs list.

| Field | Default | Meaning |
|---|---|---|
| `enabled` | true | Master switch. Off keeps the in-app feed but sends nothing. |
| `warning_debounce` | 5m | How long a warning must persist before it is sent. |
| `digest_at` | 09:00 | `HH:MM` the info digest is sent. |
| `timezone` | system | IANA zone for `digest_at`. |
| `renotify_on_escalation` | true | Send again when severity rises. |
| `notify_resolved` | true | Send resolved messages. |
| `retention` | 30d | How long resolved alerts and events stay in the feed. |
| `public_url` | cluster web address | Base for links in messages. |
| `thresholds.disk_warning_percent` / `disk_critical_percent` | 90 / 95 | Drive usage for `node.disk_low`. |
| `thresholds.memory_percent` / `memory_for` | 92 / 10m | Sustained RAM use for `node.memory_pressure`. |
| `thresholds.cert_warning_days` / `cert_critical_days` | 14 / 7 | Days left for `cert.expiring`. |
| `events.<key>.severity` | catalog | Override an event's severity. |
| `events.<key>.route` | default | `muted`, `in_app` (feed only) or a list of destination names (exactly those, ignoring their filters). |

### Destinations

Each destination has a unique `name` (`[a-z0-9-]`, up to 40 characters), an
`enabled` flag and these filters, all optional: `severities` (sent the moment
they fire; default warning and critical), `digest`, `categories` and `stacks`
(cluster-wide alerts always pass the stack filter).

| Kind | Needs |
|---|---|
| `webhook` | An http(s) URL, optionally an `Authorization` header value. |
| `discord` | A discord.com / discordapp.com webhook URL (ptb and canary included). Options: `style` and `mention` (`<@&role>`, `<@user>`, `@here`, `@everyone`). |
| `email` | `to` addresses, and the `smtp` server in the settings. |

URLs, authorization values and the SMTP password are **sealed secrets**
(`_clouduler_alert_<name>_url`, `_clouduler_alert_<name>_auth`,
`_clouduler_alert_smtp_password`). They are write-only: the API reports the
scheme and host of a stored URL (`https://discord.com/...`) and whether an
authorization value is set, never the value. Sending the masked string back
keeps the stored URL. Deleting a destination deletes its secrets. Error
messages, health records and logs never contain a URL path or token.

## Events

Severity is the default; override it per event in the settings. "state"
events open and resolve, "event" ones are one-shot. "stack" events can be
muted per stack (below).

### Cluster

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `cluster.quorum_lost` | critical | state | Quorum lost. Sent directly by the managers, since nothing can be written to raft without quorum |
| `cluster.emergency_mode` | critical | state | Emergency mode is on.  |
| `cluster.leader_changed` | info | event | Raft leader changed. Warning if it happens more than 3 times in 10 minutes |
| `cluster.node_joined` | info | event | Node joined.  |
| `cluster.node_removed` | info | event | Node removed.  |

### Nodes

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `node.unreachable` | critical | state | Node unreachable. Warning for a worker |
| `node.disk_low` | warning | state | Drive folder low on space. Critical past the critical threshold |
| `node.memory_pressure` | warning | state | Sustained memory pressure.  |

### Workloads

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `instance.crash_loop` | critical | state, stack | Container is crash looping.  |
| `instance.oom_killed` | warning | event, stack | Container killed for using too much memory. Critical if it happens 3 times within an hour |
| `instance.exited` | warning | state, stack | Container not in its desired state.  |
| `instance.unhealthy` | warning | state, stack | Container failing its healthcheck.  |
| `instance.unplaceable` | warning | state, stack | No node can run this container.  |
| `instance.unreachable_by_proxy` | warning | state, stack | The proxy cannot reach a container.  |
| `instance.pin_drift` | info | state, stack | Container runs on another node than its pin.  |
| `log.match` | warning | event, stack | A watched line appeared in a container's logs. Severity comes from the stack's log rule |

### Storage

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `volume.sync_refused` | critical | state, stack | Sync refused because it would wipe data.  |
| `volume.migration_failed` | critical | state, stack | Volume migration failed.  |
| `volume.empty` | critical | state, stack | A volume that should hold data is empty.  |
| `volume.replica_error` | warning | state, stack | A replica has an error or is behind.  |
| `volume.migration_completed` | info | event, stack | Volume migration completed.  |

### Backups

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `restore.failed` | critical | state | A restore failed.  |
| `backup.failed` | warning | state | A backup failed. Critical if it keeps failing past 24 hours |
| `backup.overdue` | warning | state | No backup succeeded within twice its interval.  |
| `backup.succeeded_with_warning` | info | event | A backup succeeded with a warning.  |

### Edge

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `cert.expiring` | warning | state | Certificate expires soon. Critical close to expiry |
| `cert.issuance_failed` | warning | state | Certificate issuance failing.  |
| `dns.reconcile_failed` | warning | state | DNS records could not be updated.  |

### Deployments

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `self_update.rolled_back` | critical | event | A node went back to its previous binary. It now runs a different version from the rest |
| `self_update.completed` | info | event | Clouduler updated on a node.  |
| `update.failed` | warning | state, stack | An image update failed.  |
| `update.stuck` | warning | state, stack | An image update is stuck.  |
| `update.completed` | info | event, stack | An image update completed.  |
| `stack.deployed` | info | event, stack | Stack deployed.  |
| `stack.removed` | info | event, stack | Stack removed.  |

### Security

| Event | Default | Kind | Meaning |
|---|---|---|---|
| `auth.api_key_created` | info | event | API key created.  |
| `auth.api_key_revoked` | info | event | API key revoked.  |
| `secret.rotated` | info | event | A secret was overwritten.  |

## Webhook payload

`POST` with `Content-Type: application/json`, headers `X-Clouduler-Event`
and `X-Clouduler-Severity`, and `Authorization` if configured. Schema
version 1:

```json
{
  "version": 1,
  "cluster": "home",
  "type": "alert",
  "event": "instance.crash_loop",
  "category": "workloads",
  "severity": "critical",
  "phase": "opened",
  "id": "a1b2",
  "title": "web.app.0 is crash-looping",
  "detail": "...",
  "subject": "web.app.0",
  "stack": "web", "instance": "web.app.0", "node": "n1",
  "opened_at": 1700000000, "at": 1700000300, "count": 3,
  "link": "https://cloud.example.com/instances/web.app.0",
  "links": [{"label": "Open instance", "url": "..."}],
  "excerpt": ["last log lines"],
  "ui_url": "https://cloud.example.com"
}
```

`phase` is `opened`, `escalated` (with `previous_severity`) or `resolved`.
`type` is `alert`, `digest` (`count` and an `alerts` array of the same
objects) or `test`.

## Per stack: `x-clouduler.alerts`

At stack level, and again per service (the service block adds to the stack
block; `enabled` and `severity` override). See `docs/stacks.md`.

```yaml
x-clouduler:
  alerts:
    mute: [instance.unhealthy, volume.*]
    severity: {instance.exited: critical}
    logs:
      - match: "FATAL"
        severity: critical
        services: [app]
      - regex: 'panic: .*'
        cooldown: 10m
```

`alerts: false` turns every alert of the stack (or service) off. Keys must
exist in the catalog; `volume.*` mutes a category prefix.

### Log rules

Each rule has exactly one of `match` (plain substring) and `regex` (Rust
regex syntax, matched anywhere in the line), plus `ignore_case`, `severity`
(default warning), `title`, `cooldown` (default 5m; matches inside it are
only counted), `stream` (`both`, `stdout`, `stderr`) and, at stack level,
`services`. Patterns are at most 512 characters, 32 rules per block. Matches
raise `log.match` with the surrounding lines as the excerpt.

Performance: only containers whose stack has rules are followed, one tail
per container on the node running it, and rules are compiled once. A
container that logs very heavily costs one regex pass per line; keep
patterns simple and use `stream` and `services` to narrow them.

## API

See `docs/api.md`: `GET /api/alerts`, `GET /api/alerts/catalog`,
`GET`/`PUT /api/alerts/settings`, `POST /api/alerts/test`.
