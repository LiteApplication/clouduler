import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

const config: Config = {
  title: 'Clouduler',
  tagline: 'The easiest way to run a homelab cluster',
  favicon: 'img/favicon.svg',
  url: 'https://clouduler.example.com',
  baseUrl: '/',
  onBrokenLinks: 'throw',
  onBrokenAnchors: 'warn',
  markdown: {
    mermaid: true,
    hooks: {onBrokenMarkdownLinks: 'throw'},
  },
  i18n: {defaultLocale: 'en', locales: ['en']},
  themes: [
    '@docusaurus/theme-mermaid',
    [
      '@easyops-cn/docusaurus-search-local',
      {
        hashed: true,
        indexBlog: false,
        docsRouteBasePath: '/docs',
        docsDir: 'content',
        highlightSearchTermsOnTargetPage: true,
      },
    ],
  ],
  presets: [
    [
      'classic',
      {
        docs: {
          path: 'content',
          routeBasePath: '/docs',
          sidebarPath: './sidebars.ts',
        },
        blog: false,
        theme: {customCss: './src/css/custom.css'},
      } satisfies Preset.Options,
    ],
  ],
  themeConfig: {
    colorMode: {defaultMode: 'dark', respectPrefersColorScheme: true},
    navbar: {
      title: 'Clouduler',
      logo: {alt: 'Clouduler', src: 'img/logo.svg'},
      items: [
        {type: 'docSidebar', sidebarId: 'docs', position: 'left', label: 'Docs'},
        {to: '/docs/quickstart/single-node', label: 'Quick start', position: 'left'},
        {to: '/docs/reference/cluster-yaml', label: 'Reference', position: 'left'},
      ],
    },
    footer: {
      style: 'dark',
      links: [
        {
          title: 'Learn',
          items: [
            {label: 'What is Clouduler', to: '/docs/intro/what-is-clouduler'},
            {label: 'How it works', to: '/docs/intro/how-it-works'},
            {label: 'Glossary', to: '/docs/intro/glossary'},
          ],
        },
        {
          title: 'Start',
          items: [
            {label: 'Single node', to: '/docs/quickstart/single-node'},
            {label: 'LAN cluster', to: '/docs/quickstart/lan-cluster'},
            {label: 'Multi-site', to: '/docs/quickstart/multi-site-headscale'},
          ],
        },
        {
          title: 'More',
          items: [
            {label: 'Examples', to: '/docs/examples'},
            {label: 'Troubleshooting', to: '/docs/reference/troubleshooting'},
          ],
        },
      ],
      copyright: `Clouduler documentation. Built with Docusaurus.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ['yaml', 'bash', 'toml', 'json', 'ini', 'nginx', 'docker', 'rust'],
    },
    mermaid: {theme: {light: 'neutral', dark: 'dark'}},
  } satisfies Preset.ThemeConfig,
};

export default config;
