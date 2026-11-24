// The individual configuration pages. Each one edits a slice of the draft
// config it is handed; none of them own state, so switching pages never
// loses an edit because the draft lives in the Configuration shell above them.

export type { SectionProps } from './types';
export { ClusterSection } from './Cluster';
export { renameNetworkInDraft, NetworksSection } from './Networks';
export { DrivesSection } from './Drives';
export { NodeCard } from './Nodes';
export { emptyContainer, serviceNamesOf } from './NetworkFields';
export { WebSection } from './Web';
export { FailoverSection } from './Failover';
export { SecretsSection } from './Secrets';
export { GitBackupSection } from './GitBackup';
export { TerraformSection } from './Terraform';
export { McpSection } from './Mcp';
export { AuthSection } from './Auth';
export { ServiceLoginSection } from './ServiceLogin';
export { ProxySection } from './Proxy';
export { parseUpstreamList } from './Dns';
export { CloudflareSection } from './Cloudflare';
