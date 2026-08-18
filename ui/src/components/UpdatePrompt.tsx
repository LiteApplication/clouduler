// Registers the service worker and, when it has a newer build waiting, offers
// a reload instead of swapping the app out from under whatever the operator
// is in the middle of.

import { useEffect, useState } from 'react';
import { registerServiceWorker } from '../pwa';
import { Icon } from './Icon';

export function UpdatePrompt() {
  const [apply, setApply] = useState<(() => void) | null>(null);

  useEffect(() => {
    // Wrapped in a thunk: setState treats a bare function as an updater.
    registerServiceWorker((doApply) => setApply(() => doApply));
  }, []);

  if (!apply) return null;

  return (
    <div className="toast" role="status">
      <Icon name="download" />
      <span>A new version is ready.</span>
      <button className="primary" onClick={apply}>
        Reload
      </button>
      <button className="ghost icon-btn" onClick={() => setApply(null)} aria-label="Dismiss">
        <Icon name="close" />
      </button>
    </div>
  );
}
