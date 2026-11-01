import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api } from '../../api/client';
import type { AlertDestination, AlertSettingsResponse } from '../../api/types';
import { ToastProvider } from '../../ds';
import { formFrom } from '../../lib/alerts';
import { DestinationSheet } from './DestinationSheet';

const dest = { name: 'pager', kind: 'webhook', severities: ['critical'], categories: [] } as unknown as AlertDestination;

function settings(): AlertSettingsResponse {
  return {
    settings: { destinations: [dest] },
    secrets: { pager: { url: 'https://ntfy.example/…', auth_set: true } },
    smtp_password: false,
    health: {},
  } as unknown as AlertSettingsResponse;
}

function open() {
  render(
    <ToastProvider>
      <DestinationSheet initial={formFrom(dest)} settings={settings()} onClose={() => {}} onSaved={async () => {}} onOpenSmtp={() => {}} />
    </ToastProvider>,
  );
}

describe('DestinationSheet sealed values', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('shows the masked URL and never sends it back', async () => {
    const put = vi.spyOn(api, 'putAlertSettings').mockResolvedValue(undefined as never);
    open();
    expect(screen.getByText('https://ntfy.example/…')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(put).toHaveBeenCalled());
    const body = put.mock.calls[0][0];
    expect(JSON.stringify(body)).not.toContain('ntfy.example');
    expect(body.secrets).toBeUndefined();
  });

  it('removes the auth header only on an explicit Remove', async () => {
    const put = vi.spyOn(api, 'putAlertSettings').mockResolvedValue(undefined as never);
    open();
    fireEvent.click(screen.getByRole('button', { name: 'Remove' }));
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(put).toHaveBeenCalled());
    expect(put.mock.calls[0][0].secrets).toEqual({ pager: { auth: null } });
  });
});
