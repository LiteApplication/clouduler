import { describe, expect, it } from 'vitest';
import { cursorInstallLink, vscodeInstallLink } from './SettingsAccess';

describe('install deep links', () => {
  it('encodes the cursor config as base64 json', () => {
    const link = cursorInstallLink('clouduler', 'https://c.example/mcp');
    const url = new URL(link);
    expect(url.searchParams.get('name')).toBe('clouduler');
    expect(JSON.parse(atob(url.searchParams.get('config')!))).toEqual({ url: 'https://c.example/mcp' });
  });

  it('url-encodes the whole vscode server definition', () => {
    const link = vscodeInstallLink('clouduler', 'https://c.example/mcp');
    expect(link.startsWith('vscode:mcp/install?')).toBe(true);
    expect(JSON.parse(decodeURIComponent(link.split('?')[1]))).toEqual({
      name: 'clouduler',
      type: 'http',
      url: 'https://c.example/mcp',
    });
  });
});
