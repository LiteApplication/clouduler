import { setTransport } from '../api/transport';
import type { MockDb } from './db';
import { buildScenario, DEFAULT_SCENARIO } from './scenarios';
import { createMockTransport, type MockTransportOptions } from './transport';

/** Build a scenario and route every network call of the app to it. */
export function installMock(scenario: string = DEFAULT_SCENARIO, opts: MockTransportOptions = {}): MockDb {
  const db = buildScenario(scenario);
  setTransport(createMockTransport(db, opts));
  return db;
}
