// Placeholder: Agent C replaces this with the single Backups page. Until then
// the old backup pages are stacked on it.
import { Destinations } from '../backup/Destinations';
import { Schedules } from '../backup/Schedules';
import { History } from '../backup/History';
import { RestoreWizard } from '../backup/RestoreWizard';

export function Backups() {
  return (
    <>
      <Schedules />
      <History />
      <Destinations />
      <RestoreWizard />
    </>
  );
}
