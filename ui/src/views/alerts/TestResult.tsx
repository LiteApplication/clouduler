import { Icon } from '../../ds';
import type { AlertTestResult } from '../../api/types';

export function TestResult({ result }: { result: AlertTestResult | null }) {
  if (!result) return null;
  return result.ok ? (
    <span className="al-testres al-testres--ok" role="status">
      <Icon name="check-circle" size={16} /> Delivered. Check the destination.
    </span>
  ) : (
    <span className="al-testres al-testres--bad" role="alert">
      <Icon name="x" size={16} /> {result.error ?? 'The test failed.'}
    </span>
  );
}
