// The two network primitives the app uses, behind one swappable seam: a
// request/response `fetch` and a server-sent-events `eventSource`. Production
// uses the browser globals (looked up at call time, so tests that stub
// `globalThis.fetch` keep working). The docs-site build installs an in-memory
// fake backend here instead; see src/mock.

/** The slice of EventSource the app touches. */
export interface EventSourceLike {
  onopen: ((evt: Event) => void) | null;
  onerror: ((evt: Event) => void) | null;
  onmessage: ((evt: MessageEvent) => void) | null;
  addEventListener(type: string, listener: (evt: MessageEvent) => void): void;
  removeEventListener(type: string, listener: (evt: MessageEvent) => void): void;
  close(): void;
}

export interface Transport {
  fetch(input: string, init?: RequestInit): Promise<Response>;
  eventSource(url: string): EventSourceLike;
}

const browserTransport: Transport = {
  fetch: (input, init) => globalThis.fetch(input, init),
  eventSource: (url) => new EventSource(url) as unknown as EventSourceLike,
};

let current: Transport = browserTransport;

export function getTransport(): Transport {
  return current;
}

/** Install a transport; call with no argument to restore the browser one. */
export function setTransport(transport?: Transport) {
  current = transport ?? browserTransport;
}
