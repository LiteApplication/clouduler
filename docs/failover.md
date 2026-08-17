# Node failure and failover

Two independent mechanisms handle a node dropping out. They are timed against
each other, and getting that ordering wrong is the one way to lose data.

## Rescheduling: the cluster gives up on a node

The raft leader probes every node in the roster every `probe_interval`. When a
node has not answered for longer than `node_timeout`, the leader submits
`SetReachable { reachable: false }` through raft. An unreachable node is not a
placement candidate, so the next scheduling pass moves its instances elsewhere,
subject to the usual rule that the target must implement every drive the
instance needs.

When the node answers again, the leader clears the flag and the node becomes a
candidate for new work. Instances already moved away do not move back on their
own; that is an operator decision made from the web ui.

The tracker is per leader and starts empty. A new leader therefore waits a full
`node_timeout` before declaring anyone dead, which is deliberate: the moments
right after an election are when probe results are least trustworthy.

## Fencing: a node gives up on itself

Every node watches its own quorum. Once it has been without one for longer than
`fence_timeout`, it stops the containers it is running. When quorum returns, the
reconcile loop starts them again.

A node needs this because it cannot tell a partition from a cluster that has
moved on without it. If it kept running containers while the rest of the cluster
handed them to someone else, a replicated volume would have two writers.

Quorum for a leader means a majority of voters acknowledged it within the last
1.5 seconds. For a follower it means both that a leader is known and that
traffic from that leader arrived recently. The second half matters: a follower,
and a learner in particular, keeps reporting the last leader it saw long after
that node has gone, because nothing replaces it. Timing the arrival of leader
traffic is what lets an isolated node work out that it is alone.

A brief quorum loss does not fence. The clock restarts on every recovery, so
flapping keeps containers up; only a sustained loss stops them.

## Configuration

```yaml
failover:
  node_timeout: 30s    # unreachable for this long, work is rescheduled
  fence_timeout: 15s   # no quorum for this long, local containers stop
  probe_interval: 5s   # how often the leader probes
```

The config is rejected unless `probe_interval < fence_timeout < node_timeout`.

The gap between `fence_timeout` and `node_timeout` is the safety margin: it must
cover the time a failing node needs to actually stop its containers. Fifteen
seconds is comfortable for most workloads. Shrinking it makes failover faster
and the margin thinner; widening `node_timeout` makes failover slower but safer.

`probe_interval` bounds how late a failure is noticed, and must be shorter than
`fence_timeout` so a node cannot be declared dead before it has had the chance
to fence.
