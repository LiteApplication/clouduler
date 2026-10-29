// Placeholder: Agent B builds the Logs screen (container and node-daemon logs).
import { EmptyState, PageHeader } from '../../ds';

export function Logs() {
  return (
    <>
      <PageHeader eyebrow="Cluster" title="Logs" />
      <EmptyState icon="terminal" title="Logs are on their way">
        Open a container from Stacks to read its logs in the meantime.
      </EmptyState>
    </>
  );
}
