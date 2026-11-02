// Entry point of the docs-site build (mock.html). Same providers as main.tsx,
// but the network is an in-memory fake backend and routing lives in memory so
// the page can be embedded anywhere (an iframe on a sub-path, say).
//
// Query params:
//   scenario  one of src/mock/scenarios (default lan-3-nodes)
//   route     initial in-app route, e.g. /stacks/nextcloud (default /)
//   theme     light | dark (default: follow the OS)
//   chrome    1 = full app shell (default), 0 = hide nav so one page shows

import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { MemoryRouter } from 'react-router-dom';
import { App } from './App';
import { AuthProvider } from './auth/AuthContext';
import { installMock } from './mock/install';
import { DEFAULT_SCENARIO, scenarios } from './mock/scenarios';
import { applyTheme, THEME_STORAGE_KEY } from './theme';
import '@fontsource/bricolage-grotesque/latin-600.css';
import '@fontsource/bricolage-grotesque/latin-700.css';
import '@fontsource/bricolage-grotesque/latin-800.css';
import '@fontsource/hanken-grotesk/latin-400.css';
import '@fontsource/hanken-grotesk/latin-500.css';
import '@fontsource/hanken-grotesk/latin-600.css';
import '@fontsource/hanken-grotesk/latin-700.css';
import '@fontsource/jetbrains-mono/latin-400.css';
import '@fontsource/jetbrains-mono/latin-500.css';
import '@fontsource/jetbrains-mono/latin-600.css';
import './styles.css';
import './ds/ds.css';
import './ds/layout.css';
import './mock/mock.css';

const params = new URLSearchParams(window.location.search);
const scenario = params.get('scenario') ?? DEFAULT_SCENARIO;
if (!(scenario in scenarios)) {
  document.body.textContent = `Unknown scenario "${scenario}". Available: ${Object.keys(scenarios).join(', ')}`;
  throw new Error(`unknown mock scenario ${scenario}`);
}
const route = params.get('route') ?? '/';
const theme = params.get('theme');

if (theme === 'light' || theme === 'dark') {
  // useTheme reads the stored choice, so persist it before first render.
  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Blocked storage (embedded, private mode): the stamp below still applies once.
  }
  applyTheme(theme);
}
if (params.get('chrome') === '0') document.body.classList.add('mock-nochrome');

installMock(scenario, { latencyMs: 60 });

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <MemoryRouter initialEntries={[route]}>
      <AuthProvider>
        <App />
      </AuthProvider>
    </MemoryRouter>
  </StrictMode>,
);
