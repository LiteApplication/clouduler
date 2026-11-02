import React from 'react';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';

export default function NotFound(): React.JSX.Element {
  return (
    <Layout title="Page not found">
      <main className="container">
        <div className="nf-wrap">
          <p className="nf-code">404</p>
          <h1>This node has left the cluster.</h1>
          <p>The page you wanted moved or never existed. Failover did not find a replica.</p>
          <div style={{display: 'flex', gap: '1rem', justifyContent: 'center', flexWrap: 'wrap'}}>
            <Link className="button button--primary button--lg" to="/">Back home</Link>
            <Link className="button button--secondary button--lg" to="/docs/intro/what-is-clouduler">Browse the docs</Link>
          </div>
        </div>
      </main>
    </Layout>
  );
}
