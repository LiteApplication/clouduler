import { Link, useNavigate } from 'react-router-dom';
import { useClusterState } from '../state/store';
import { useAuth } from '../auth/AuthContext';

export function Stacks() {
  const cluster = useClusterState();
  const { isAdmin } = useAuth();
  const navigate = useNavigate();

  if (!cluster) return <p className="muted">Loading...</p>;
  const stacks = Object.values(cluster.stacks);

  return (
    <div>
      <div className="page-header">
        <h1>Stacks</h1>
        {isAdmin && (
          <button className="primary" onClick={() => navigate('/stacks/_new')}>
            Deploy stack
          </button>
        )}
      </div>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Revision</th>
              <th>Services</th>
              <th>Instances</th>
            </tr>
          </thead>
          <tbody>
            {stacks.map((s) => {
              const instanceCount = Object.keys(cluster.assignments).filter((id) => id.startsWith(`${s.name}.`)).length;
              return (
                <tr key={s.name}>
                  <td>
                    <Link className="row-link" to={`/stacks/${encodeURIComponent(s.name)}`}>
                      {s.name}
                    </Link>
                  </td>
                  <td>{s.revision}</td>
                  <td>{countServices(s.spec)}</td>
                  <td>{instanceCount}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// Cheap service count for the list view without a full yaml parser: count
// top level keys under `services:`. StackDetail shows the real spec.
function countServices(spec: string): number {
  const lines = spec.split('\n');
  const idx = lines.findIndex((l) => /^services:\s*$/.test(l));
  if (idx === -1) return 0;
  let count = 0;
  for (let i = idx + 1; i < lines.length; i++) {
    const line = lines[i];
    if (/^\S/.test(line)) break;
    if (/^  \S[^:]*:\s*$/.test(line) || /^  \S[^:]*:\s*\{/.test(line)) count++;
  }
  return count;
}
