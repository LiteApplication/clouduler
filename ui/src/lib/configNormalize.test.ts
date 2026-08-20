import { describe, expect, it } from 'vitest';
import type { ClusterConfig } from '../api/types';
import { normalizeConfig } from './configNormalize';
import { diffConfig } from './configDiff';

// The body the API actually returns for a cluster that has never configured
// OIDC or Cloudflare: serde leaves the unset optional keys out entirely.
const raw = JSON.parse(`{"cluster":{"name":"brimacluster"},"networks":{"internal":{"type":"internal","subnet":"100.64.0.0/10","domain":"home.liteapp.fr"}},"drives":{"bulk-media":{"sync_interval":"15m","mode":"replicated"}},"nodes":{"piosny":{"role":"manager","network":{"internal":"100.64.0.2"},"drives":{"bulk-media":"/srv/media"},"rpc_port":7373,"data_dir":"/var/lib/clouduler/"}},"web":{"address":"cloud","network":"internal","port":8443},"data_dir":"/var/lib/clouduler","failover":{"node_timeout":"30s","fence_timeout":"15s","probe_interval":"5s"},"secrets":{"key_file":"/etc/clouduler/secrets.key","generate_if_missing":true},"auth":{"api_keys":true,"session_ttl":"12h"},"proxy":{"enabled":true,"http_port":80,"https_port":443,"acme":{"enabled":false}}}`) as ClusterConfig;

describe('normalizeConfig', () => {
  it('turns omitted optional sections into explicit nulls', () => {
    const cfg = normalizeConfig(raw);
    expect(cfg.auth.oidc).toBeNull();
    expect(cfg.cloudflare).toBeNull();
    expect(cfg.proxy.acme.directory_url).toBeNull();
    expect(cfg.drives['bulk-media'].served_by).toEqual([]);
    expect(cfg.nodes.piosny.generate_secret_key).toBeNull();
  });

  it('keeps values that are present', () => {
    const cfg = normalizeConfig(raw);
    expect(cfg.cluster.name).toBe('brimacluster');
    expect(cfg.networks.internal.subnet).toBe('100.64.0.0/10');
    expect(cfg.nodes.piosny.data_dir).toBe('/var/lib/clouduler/');
  });

  it('is stable, so a freshly loaded draft shows no staged changes', () => {
    const cfg = normalizeConfig(raw);
    expect(diffConfig(cfg, normalizeConfig(JSON.parse(JSON.stringify(cfg))))).toEqual([]);
  });
});
