// Registers the service worker and, when it has a newer build waiting, offers
// a reload instead of swapping the app out from under whatever the operator
// is in the middle of.

import { useEffect, useState } from 'react';
import { Toast } from '../ds';
import { registerServiceWorker } from '../pwa';

export function UpdatePrompt() {
  const [apply, setApply] = useState<(() => void) | null>(null);

  useEffect(() => {
    // Wrapped in a thunk: setState treats a bare function as an updater.
    registerServiceWorker((doApply) => setApply(() => doApply));
  }, []);

  if (!apply) return null;

  return (
    <div className="cl-toasts">
      <Toast tone="info" title="A new version is ready." action="Reload" onAction={apply} onClose={() => setApply(null)} />
    </div>
  );
}
