import React from 'react';
import Link from '@docusaurus/Link';
import Layout from '@theme/Layout';
import UiScreen from '@site/src/components/UiScreen';
import styles from './index.module.css';

const FEATURES = [
  {t: 'Stacks, not YAML sprawl', d: 'Describe a service once. Clouduler places it, keeps it running, and updates container images and Clouduler itself when you ask.'},
  {t: 'Automatic failover', d: 'A node dies, its stacks restart on another one. The cut-off node stops its own containers first, so a volume never has two writers.'},
  {t: 'Built-in proxy and TLS', d: "Publish a service on your domain with Let's Encrypt certificates and Cloudflare DNS records handled for you, plus an optional login in front."},
  {t: 'Drives and backups', d: 'Replicate volumes across nodes or serve one over NFS, then back drives up on a schedule to encrypted restic repositories you can restore.'},
  {t: 'Private by default', d: 'Containers talk over an overlay between nodes, private-network routes refuse outside clients, and secrets are encrypted in the cluster state.'},
  {t: 'Alerts', d: 'Get notified about offline nodes, full disks and crash loops on Discord, a JSON webhook or email. Info events batch into a daily digest.'},
  {t: 'AI access over MCP', d: 'Off by default. Turn it on to let Claude Code or Cursor inspect and debug the cluster over OAuth, as read-only or admin, with secrets redacted.'},
  {t: 'Git backup', d: 'Mirror cluster config and stacks to your own Git repository, with secrets age-encrypted. Edit a stack in Git and a webhook pulls it in.'},
  {t: 'Web UI, CLI and API', d: 'Use the web UI, the CLI or the HTTP API it is built on. Most config changes apply live, with no restart.'},
];

const STARTS = [
  {to: '/docs/quickstart/single-node', k: 'One machine', t: 'Single node', d: 'Start with one box and a first stack running in minutes.'},
  {to: '/docs/quickstart/lan-cluster', k: 'Same network', t: 'LAN cluster', d: 'Join more nodes and watch services survive a failure.'},
  {to: '/docs/quickstart/multi-site-headscale', k: 'Different places', t: 'Multi-site', d: 'Link houses and VPS nodes over a Headscale mesh.'},
];

export default function Home(): React.JSX.Element {
  return (
    <Layout title="Clouduler" description="The easiest way to run a homelab cluster">
      <header className={styles.hero}>
        <div className={styles.heroInner}>
          <span className={styles.pill}>Self-hosted container cluster</span>
          <h1 className={styles.title}>
            The easiest way to run a <span className={styles.mark}>homelab</span> cluster.
          </h1>
          <p className={styles.sub}>
            Clouduler turns a few spare machines into one reliable platform. A Rust daemon, a friendly web UI, and
            no Kubernetes degree required.
          </p>
          <div className={styles.cta}>
            <Link className="button button--primary button--lg" to="/docs/quickstart/single-node">Get started</Link>
            <Link className="button button--secondary button--lg" to="/docs/intro/what-is-clouduler">What is Clouduler?</Link>
          </div>
        </div>
        <div className={styles.shot}>
          <UiScreen scenario="lan-3-nodes" route="/stacks" alt="The Clouduler stacks view" height={420} />
        </div>
      </header>
      <main className={styles.main}>
        <section className={styles.grid}>
          {FEATURES.map((f) => (
            <div key={f.t} className={styles.card}>
              <h3>{f.t}</h3>
              <p>{f.d}</p>
            </div>
          ))}
        </section>
        <h2 className={styles.h2}>Pick your starting point</h2>
        <section className={styles.starts}>
          {STARTS.map((s, i) => (
            <Link key={s.to} to={s.to} className={styles.start}>
              <span className={styles.num}>{i + 1}</span>
              <span className={styles.kicker}>{s.k}</span>
              <strong>{s.t}</strong>
              <span>{s.d}</span>
            </Link>
          ))}
        </section>
      </main>
    </Layout>
  );
}
