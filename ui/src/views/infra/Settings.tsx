// Placeholder: Agent C replaces this with the tabbed Settings screen
// (/settings/:tab). Until then each tab hosts the old page that matches it; the
// config tabs mount the old Configuration view at one of its sub-pages, picked
// with Chips.
import { Navigate, Route, Routes, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { Chip, PageHeader, Tabs } from '../../ds';
import { AiAccess } from '../AiAccess';
import { ConfigsSecrets } from '../ConfigsSecrets';
import { Configuration } from '../Configuration';
import { SelfUpdate } from '../SelfUpdate';
import { CONFIG_PAGES } from '../config/pages';

export const SETTINGS_TABS = [
  { id: 'general', label: 'General' },
  { id: 'network', label: 'Network' },
  { id: 'storage', label: 'Storage' },
  { id: 'security', label: 'Security' },
  { id: 'secrets', label: 'Secrets & configs' },
  { id: 'access', label: 'Access' },
  { id: 'self-update', label: 'Self-update' },
];

/** Which settings tab owns an old /configuration/<slug> page. */
export const CONFIG_SLUG_TAB: Record<string, string> = {
  cluster: 'general',
  failover: 'general',
  review: 'general',
  networks: 'network',
  web: 'network',
  proxy: 'network',
  cloudflare: 'network',
  drives: 'storage',
  auth: 'security',
  mcp: 'security',
  'git-backup': 'security',
  secrets: 'secrets',
};

export function Settings() {
  const { tab = 'general' } = useParams();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  if (!SETTINGS_TABS.some((t) => t.id === tab)) return <Navigate to="/settings/general" replace />;

  const slugs = CONFIG_PAGES.filter((p) => CONFIG_SLUG_TAB[p.slug] === tab).map((p) => p.slug);
  const slug = slugs.includes(params.get('page') ?? '') ? params.get('page')! : slugs[0];

  return (
    <>
      <PageHeader eyebrow="System" title="Settings" />
      <Tabs label="Settings sections" items={SETTINGS_TABS} active={tab} onChange={(id) => navigate(`/settings/${id}`)} />
      {slug && (
        <>
          <div className="chips">
            {slugs.map((s) => (
              <Chip
                key={s}
                label={CONFIG_PAGES.find((p) => p.slug === s)!.label}
                selected={s === slug}
                onClick={() => setParams({ page: s })}
              />
            ))}
          </div>
          <Routes location={{ pathname: `/configuration/${slug}` }}>
            <Route path="/configuration/*" element={<Configuration />} />
          </Routes>
        </>
      )}
      {tab === 'secrets' && <ConfigsSecrets />}
      {tab === 'access' && <AiAccess />}
      {tab === 'self-update' && <SelfUpdate />}
    </>
  );
}
