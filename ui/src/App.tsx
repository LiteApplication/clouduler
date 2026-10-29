import { Navigate, Route, Routes, useLocation, useParams } from 'react-router-dom';
import { useAuth } from './auth/AuthContext';
import { ClusterStoreProvider } from './state/store';
import { InstanceProgressProvider } from './state/progressStore';
import { InstancesProvider } from './state/instancesStore';
import { ConfirmProvider, ToastProvider } from './ds';
import { Layout } from './components/Layout';
import { PhoneMore } from './views/PhoneMore';
import { ConfigDraftProvider } from './views/config/draft';
import { CONFIG_SLUG_TAB } from './views/infra/Settings';
import { Login } from './views/workloads/Login';
import { OAuthConsent } from './views/workloads/OAuthConsent';
import { Overview } from './views/workloads/Overview';
import { Stacks } from './views/workloads/Stacks';
import { StackDetail } from './views/workloads/StackDetail';
import { Deploy } from './views/workloads/Deploy';
import { Instances } from './views/workloads/Instances';
import { ContainerDetail } from './views/workloads/ContainerDetail';
import { Logs } from './views/workloads/Logs';
import { Updates } from './views/workloads/Updates';
import { Nodes } from './views/infra/Nodes';
import { NodeDetail } from './views/infra/NodeDetail';
import { Proxy } from './views/infra/Proxy';
import { Dns } from './views/infra/Dns';
import { Storage } from './views/infra/Storage';
import { DriveDetail } from './views/infra/DriveDetail';
import { NetworkDetail } from './views/infra/NetworkDetail';
import { Settings } from './views/infra/Settings';

export function App() {
  const { identity, loading } = useAuth();

  return (
    <ToastProvider>
      <ConfirmProvider>
        {loading ? (
          <p className="pageload" style={{ padding: 24 }}>
            <span className="spinner" /> Loading...
          </p>
        ) : !identity ? (
          <Login />
        ) : (
          // The consent page is a standalone approval screen reached from an MCP
          // client's browser redirect, so it renders without the app shell.
          <Routes>
            <Route path="/oauth/consent" element={<OAuthConsent />} />
            <Route path="*" element={<Shell />} />
          </Routes>
        )}
      </ConfirmProvider>
    </ToastProvider>
  );
}

/** `/configuration/<slug>` -> the Settings tab that now owns that page. */
function ConfigurationRedirect() {
  const slug = useParams()['*']?.split('/')[0] ?? '';
  const tab = CONFIG_SLUG_TAB[slug] ?? 'general';
  const page = CONFIG_SLUG_TAB[slug] && slug !== 'review' && tab !== 'secrets' ? `?page=${slug}` : '';
  return <Navigate to={`/settings/${tab}${page}`} replace />;
}

/** Redirect that keeps the query string, so deep links such as
 *  /backup/restore?scope=drive&drive=x still carry their parameters. */
function Redirect({ to }: { to: (params: Record<string, string | undefined>) => string }) {
  const params = useParams();
  const { search } = useLocation();
  return <Navigate to={`${to(params)}${search}`} replace />;
}

function Shell() {
  return (
    <ClusterStoreProvider>
      <InstanceProgressProvider>
        <InstancesProvider>
          <ConfigDraftProvider>
            <Layout>
              <Routes>
                <Route path="/" element={<Overview />} />
                <Route path="/stacks" element={<Stacks />} />
                <Route path="/stacks/new" element={<Deploy />} />
                <Route path="/stacks/:name" element={<StackDetail />} />
                <Route path="/stacks/:name/deploy" element={<Deploy />} />
                <Route path="/instances" element={<Instances />} />
                <Route path="/instances/:id" element={<ContainerDetail />} />
                <Route path="/logs" element={<Logs />} />
                <Route path="/updates" element={<Updates />} />
                <Route path="/nodes" element={<Nodes />} />
                <Route path="/nodes/:name" element={<NodeDetail />} />
                <Route path="/proxy/:tab?" element={<Proxy />} />
                <Route path="/dns" element={<Dns />} />
                <Route path="/storage/:tab?" element={<Storage />} />
                <Route path="/storage/drives/:name" element={<DriveDetail />} />
                <Route path="/networks/:name" element={<NetworkDetail />} />
                <Route path="/settings/:tab?" element={<Settings />} />
                <Route path="/more" element={<PhoneMore />} />

                {/* Old URLs, kept working for bookmarks and links in old views. */}
                <Route path="/configs-secrets" element={<Navigate to="/settings/secrets" replace />} />
                <Route path="/drives" element={<Navigate to="/storage" replace />} />
                <Route path="/drives/:name" element={<Redirect to={(p) => `/storage/drives/${p.name}`} />} />
                <Route path="/configuration/*" element={<ConfigurationRedirect />} />
                <Route path="/certificates" element={<Navigate to="/dns" replace />} />
                <Route path="/redirects" element={<Navigate to="/proxy/redirects" replace />} />
                <Route path="/backup/*" element={<Redirect to={() => '/storage/backups'} />} />
                <Route path="/self-update" element={<Navigate to="/settings/self-update" replace />} />
                <Route path="/ai-access" element={<Navigate to="/settings/access" replace />} />
                <Route path="*" element={<Navigate to="/" replace />} />
              </Routes>
            </Layout>
          </ConfigDraftProvider>
        </InstancesProvider>
      </InstanceProgressProvider>
    </ClusterStoreProvider>
  );
}
