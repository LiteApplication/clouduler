// Placeholder: Agent B replaces this with the redesigned deploy screen, which
// serves both /stacks/new and /stacks/:name/deploy. The old editor lives inside
// StackDetail (`_new` is its create mode), so until then both routes hand over to it.
import { Navigate, useParams } from 'react-router-dom';

export function Deploy() {
  const { name } = useParams();
  return <Navigate to={`/stacks/${name ? encodeURIComponent(name) : '_new'}`} replace />;
}
