import { Navigate, Route, Routes } from 'react-router-dom';
import { useAuth } from './auth/AuthContext';
import { ClusterStoreProvider } from './state/store';
import { InstanceProgressProvider } from './state/progressStore';
import { Layout } from './components/Layout';
import { Login } from './views/Login';
import { Overview } from './views/Overview';
import { Nodes } from './views/Nodes';
import { NodeDetail } from './views/NodeDetail';
import { Stacks } from './views/Stacks';
import { StackDetail } from './views/StackDetail';
import { Instances } from './views/Instances';
import { InstanceDetail } from './views/InstanceDetail';
import { ConfigsSecrets } from './views/ConfigsSecrets';
import { Drives } from './views/Drives';
import { DriveDetail } from './views/DriveDetail';
import { NetworkDetail } from './views/NetworkDetail';
import { Configuration } from './views/Configuration';
import { ConfigDraftProvider } from './views/config/draft';
import { Certificates } from './views/Certificates';
import { Redirects } from './views/Redirects';
import { SelfUpdate } from './views/SelfUpdate';
import { Updates } from './views/Updates';
import { Destinations } from './views/backup/Destinations';
import { Schedules } from './views/backup/Schedules';
import { History } from './views/backup/History';
import { RestoreWizard } from './views/backup/RestoreWizard';
import { AiAccess } from './views/AiAccess';
import { OAuthConsent } from './views/OAuthConsent';

export function App() {
  const { identity, loading } = useAuth();

  if (loading) {
    return (
      <p className="loading" style={{ padding: 24 }}>
        <span className="spinner" /> Loading...
      </p>
    );
  }
  if (!identity) return <Login />;

  // The consent page is a standalone approval screen reached from an MCP
  // client's browser redirect, so it renders without the app shell.
  return (
    <Routes>
      <Route path="/oauth/consent" element={<OAuthConsent />} />
      <Route path="*" element={<Shell mcpEnabled={identity.mcp_enabled !== false} />} />
    </Routes>
  );
}

function Shell({ mcpEnabled }: { mcpEnabled: boolean }) {
  return (
    <ClusterStoreProvider>
      <InstanceProgressProvider>
        <ConfigDraftProvider>
          <Layout>
            <Routes>
              <Route path="/" element={<Overview />} />
              <Route path="/nodes" element={<Nodes />} />
              <Route path="/nodes/:name" element={<NodeDetail />} />
              <Route path="/stacks" element={<Stacks />} />
              <Route path="/stacks/:name" element={<StackDetail />} />
              <Route path="/instances" element={<Instances />} />
              <Route path="/instances/:id" element={<InstanceDetail />} />
              <Route path="/updates" element={<Updates />} />
              <Route path="/configs-secrets" element={<ConfigsSecrets />} />
              <Route path="/drives" element={<Drives />} />
              <Route path="/drives/:name" element={<DriveDetail />} />
              <Route path="/networks/:name" element={<NetworkDetail />} />
              <Route path="/configuration/*" element={<Configuration />} />
              <Route path="/certificates" element={<Certificates />} />
              <Route path="/redirects" element={<Redirects />} />
              <Route path="/backup" element={<Navigate to="/backup/destinations" replace />} />
              <Route path="/backup/destinations" element={<Destinations />} />
              <Route path="/backup/schedules" element={<Schedules />} />
              <Route path="/backup/history" element={<History />} />
              <Route path="/backup/restore" element={<RestoreWizard />} />
              <Route path="/self-update" element={<SelfUpdate />} />
              {mcpEnabled && <Route path="/ai-access" element={<AiAccess />} />}
              <Route path="*" element={<Navigate to="/" replace />} />
            </Routes>
          </Layout>
        </ConfigDraftProvider>
      </InstanceProgressProvider>
    </ClusterStoreProvider>
  );
}
