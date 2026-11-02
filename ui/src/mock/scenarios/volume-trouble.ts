// lan-3-nodes with one volume in each state that needs a human: sync refused,
// frozen for hand edits, and one that looks empty.

import lan3Nodes from './lan-3-nodes';

export default function volumeTrouble() {
  const db = lan3Nodes();
  db.scenario = 'volume-trouble';
  db.description = 'Three volumes needing attention: Sync refused, Frozen and Looks empty.';
  const v = db.state.volumes;
  v['fast/vaultwarden'].replicas = { cirrus: 3, atlas: 2, boreas: 3 };
  v['fast/vaultwarden'].sync_refused = {
    from: 'cirrus',
    to: 'atlas',
    reason: 'the source holds 0 files while the replica holds 412',
    at: db.now - 600,
  };
  v['fast/immich-db'].frozen = { node: 'cirrus', stopped: ['immich.db.0'] };
  v['bulk/immich-library'].looks_empty = true;
  return db;
}
