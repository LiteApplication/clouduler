import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, NotLeaderError, api, setUnauthorizedHandler } from './client';

function mockFetch(response: Partial<Response> & { json?: () => Promise<unknown> }) {
  const full = {
    ok: response.status ? response.status >= 200 && response.status < 300 : true,
    status: 200,
    statusText: 'OK',
    headers: new Headers(),
    json: async () => ({}),
    ...response,
  } as Response;
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(full));
  return full;
}

afterEach(() => {
  vi.unstubAllGlobals();
  setUnauthorizedHandler(null);
});

describe('api client error handling', () => {
  it('throws ApiError with the server message on a non-2xx response', async () => {
    mockFetch({ ok: false, status: 400, json: async () => ({ error: 'bad request', detail: 'why' }) });
    await expect(api.status()).rejects.toMatchObject({ status: 400, message: 'bad request', detail: 'why' });
  });

  it('falls back to statusText when the body is not json', async () => {
    mockFetch({
      ok: false,
      status: 500,
      statusText: 'Internal Server Error',
      json: async () => {
        throw new Error('not json');
      },
    });
    await expect(api.status()).rejects.toThrow('Internal Server Error');
  });

  it('calls the unauthorized handler and throws on 401', async () => {
    mockFetch({ ok: false, status: 401 });
    const handler = vi.fn();
    setUnauthorizedHandler(handler);
    await expect(api.status()).rejects.toBeInstanceOf(ApiError);
    expect(handler).toHaveBeenCalledOnce();
  });

  it('throws NotLeaderError with the Location header on a 503', async () => {
    mockFetch({ ok: false, status: 503, headers: new Headers({ Location: 'https://n2.example.com' }) });
    await expect(api.status()).rejects.toBeInstanceOf(NotLeaderError);
    try {
      await api.status();
    } catch (e) {
      expect((e as NotLeaderError).leaderUrl).toBe('https://n2.example.com');
    }
  });

  it('treats a plain 503 with no Location as a normal error', async () => {
    mockFetch({ ok: false, status: 503, json: async () => ({ error: 'unavailable' }) });
    await expect(api.status()).rejects.toBeInstanceOf(ApiError);
  });

  it('returns parsed json on success', async () => {
    mockFetch({ status: 200, json: async () => ({ leader: 'n1', quorum: true }) });
    const status = await api.status();
    expect(status.leader).toBe('n1');
  });
});
