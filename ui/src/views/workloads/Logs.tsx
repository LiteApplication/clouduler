import { useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { nodeLogsUrl } from '../../api/client';
import type { NodeLogLine } from '../../api/types';
import { useAuth } from '../../auth/AuthContext';
import { Chip, EmptyState, LogView, PageHeader, Panel, TextField, type LogViewLine } from '../../ds';
import { useEventSource } from '../../hooks/useEventSource';
import { LogMessage } from '../../lib/ansi';
import { useInstances } from '../../state/instancesStore';
import { useClusterState } from '../../state/store';
import { clockTime, logStreamOptions, toLogViewLine, type StreamLine } from './logLines';
import './workloads.css';

type Kind = 'containers' | 'nodes';
type Stream = 'all' | 'stdout' | 'stderr';

export function Logs() {
  const { isAdmin } = useAuth();
  const [params] = useSearchParams();
  const initial = params.get('instance');
  const [kind, setKind] = useState<Kind>('containers');
  const [source, setSource] = useState<string | null>(initial);
  const [filter, setFilter] = useState('');
  const [search, setSearch] = useState('');
  const [tail, setTail] = useState(200);
  const [follow, setFollow] = useState(true);
  const [stream, setStream] = useState<Stream>('all');
  const instances = useInstances();
  const cluster = useClusterState();

  const groups = useMemo(() => {
    const q = filter.trim().toLowerCase();
    const byStack = new Map<string, string[]>();
    for (const i of instances ?? []) {
      if (i.state === 'pending_placement' || (q && !i.id.toLowerCase().includes(q))) continue;
      const stack = i.id.split('.').slice(0, -2).join('.') || i.id;
      byStack.set(stack, [...(byStack.get(stack) ?? []), i.id]);
    }
    return [...byStack.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [instances, filter]);

  const nodeNames = useMemo(() => Object.keys(cluster?.nodes ?? {}).sort(), [cluster]);
  const names = kind === 'containers' ? groups : [['Nodes', nodeNames.filter((n) => n.toLowerCase().includes(filter.trim().toLowerCase()))] as [string, string[]]];

  const switchKind = (k: Kind) => {
    setKind(k);
    setSource(null);
  };

  return (
    <div className="sec wl-page">
      <PageHeader eyebrow="Cluster" title="Logs" sub="Read what a container or a node daemon is saying." />
      <div className="chips" role="group" aria-label="Log source">
        <Chip label="Containers" icon="container" selected={kind === 'containers'} onClick={() => switchKind('containers')} />
        {isAdmin && <Chip label="Node daemons" icon="node" selected={kind === 'nodes'} onClick={() => switchKind('nodes')} />}
      </div>

      <div className="split">
        <div className="split__side">
          <TextField label={kind === 'containers' ? 'Find a container' : 'Find a node'} type="search" value={filter} onChange={(e) => setFilter(e.target.value)} />
          <div className="list" role="listbox" aria-label="Log sources">
            {names.every(([, items]) => items.length === 0) && <p className="sub wl-pad">Nothing matches.</p>}
            {names.map(([group, items]) =>
              items.length === 0 ? null : (
                <div key={group}>
                  <div className="li__group eyebrow">{group}</div>
                  {items.map((item) => (
                    <button
                      key={item}
                      type="button"
                      role="option"
                      aria-selected={source === item}
                      className={`li li--btn wl-src${source === item ? ' li--sel' : ''}`}
                      onClick={() => setSource(item)}
                    >
                      <span className="li__name mono">{item}</span>
                    </button>
                  ))}
                </div>
              ),
            )}
          </div>
        </div>

        <div className="split__main">
          {!source ? (
            <EmptyState icon="terminal" title="Pick something to read">
              {kind === 'containers' ? 'Choose a container on the left to see its output.' : 'Choose a node on the left to see its daemon log.'}
            </EmptyState>
          ) : (
            <Panel title={source}>
              <div className="chips" role="group" aria-label="Log options">
                {kind === 'containers' && (
                  <>
                    <Chip label="All output" selected={stream === 'all'} onClick={() => setStream('all')} />
                    <Chip label="Output" selected={stream === 'stdout'} onClick={() => setStream('stdout')} />
                    <Chip label="Errors" selected={stream === 'stderr'} onClick={() => setStream('stderr')} />
                  </>
                )}
                <Chip label="Last 200" selected={tail === 200} onClick={() => setTail(200)} />
                <Chip label="Last 2000" selected={tail === 2000} onClick={() => setTail(2000)} />
                {kind === 'containers' && <Chip label="Follow" icon="sync" selected={follow} onClick={() => setFollow((f) => !f)} />}
              </div>
              <TextField label="Search this log" type="search" value={search} onChange={(e) => setSearch(e.target.value)} />
              {kind === 'containers' ? (
                <ContainerLog key={`${source}-${tail}-${follow}`} id={source} tail={tail} follow={follow} stream={stream} search={search} />
              ) : (
                <NodeLog key={`${source}-${tail}`} node={source} tail={tail} search={search} />
              )}
            </Panel>
          )}
        </div>
      </div>
    </div>
  );
}

function ContainerLog({ id, tail, follow, stream, search }: { id: string; tail: number; follow: boolean; stream: Stream; search: string }) {
  const url = `/api/instances/${encodeURIComponent(id)}/logs?follow=${follow}&tail=${tail}`;
  const { items, connected } = useEventSource<StreamLine>(url, logStreamOptions(2000));
  const q = search.toLowerCase();
  const visible = items.filter((l) => (stream === 'all' || l.stream === stream) && (!q || l.message.toLowerCase().includes(q)));
  return (
    <div className="biglog">
      {visible.length === 0 ? (
        <p className="sub" role="status">
          {items.length > 0 ? 'No lines match.' : connected ? 'No log output yet.' : 'Connecting to the log stream...'}
        </p>
      ) : (
        <LogView lines={visible.map(toLogViewLine)} label={`Logs for ${id}`} />
      )}
    </div>
  );
}

function NodeLog({ node, tail, search }: { node: string; tail: number; search: string }) {
  const { items, connected } = useEventSource<NodeLogLine>(nodeLogsUrl(node, tail), {
    limit: 2000,
    events: ['log'],
    dedupeKey: (l) => `${l.timestamp}|${l.target}|${l.message}`,
  });
  const q = search.toLowerCase();
  const lines: LogViewLine[] = items
    .filter((l) => !q || l.message.toLowerCase().includes(q) || l.target.toLowerCase().includes(q))
    .map((l) => ({
      t: clockTime(l.timestamp),
      level: l.level === 'trace' ? 'debug' : l.level,
      msg: (
        <>
          <span className="muted">{l.target} </span>
          <LogMessage text={l.message} />
        </>
      ),
    }));
  return (
    <div className="biglog">
      {lines.length === 0 ? (
        <p className="sub" role="status">
          {items.length > 0 ? 'No lines match.' : connected ? 'No log output yet.' : `Connecting to the daemon on ${node}...`}
        </p>
      ) : (
        <LogView lines={lines} label={`Daemon log for ${node}`} />
      )}
    </div>
  );
}
