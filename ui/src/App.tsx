import { Navigate, Route, Routes } from 'react-router-dom';
import { useAuth } from './auth/AuthContext';
import { ClusterStoreProvider } from './state/store';
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
import { Configuration } from './views/Configuration';
import { ConfigDraftProvider } from './views/config/draft';
import { Certificates } from './views/Certificates';
import { Redirects } from './views/Redirects';
import { SelfUpdate } from './views/SelfUpdate';

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

  return (
    <ClusterStoreProvider>
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
            <Route path="/configs-secrets" element={<ConfigsSecrets />} />
            <Route path="/drives" element={<Drives />} />
            <Route path="/configuration/*" element={<Configuration />} />
            <Route path="/certificates" element={<Certificates />} />
            <Route path="/redirects" element={<Redirects />} />
            <Route path="/self-update" element={<SelfUpdate />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Layout>
      </ConfigDraftProvider>
    </ClusterStoreProvider>
  );
}
