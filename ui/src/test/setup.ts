import '@testing-library/jest-dom/vitest';

// jsdom has no EventSource; tests that need one provide their own stub.
if (!('EventSource' in globalThis)) {
  // @ts-expect-error minimal stub, just enough for components that construct one
  globalThis.EventSource = class {
    onopen: (() => void) | null = null;
    onmessage: ((e: MessageEvent) => void) | null = null;
    onerror: (() => void) | null = null;
    close() {}
    addEventListener() {}
    removeEventListener() {}
  };
}
