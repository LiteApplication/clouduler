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

// jsdom has no layout; CodeMirror measures text through Range rects.
if (typeof Range !== 'undefined' && !Range.prototype.getClientRects) {
  Range.prototype.getClientRects = () => ({ length: 0, item: () => null, [Symbol.iterator]: [][Symbol.iterator] }) as unknown as DOMRectList;
  Range.prototype.getBoundingClientRect = () => new DOMRect();
}
