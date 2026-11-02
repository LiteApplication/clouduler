import React, {useEffect, useState} from 'react';
import useBaseUrl from '@docusaurus/useBaseUrl';
import {useColorMode} from '@docusaurus/theme-common';
import {uiScreenSlug} from './uiScreenSlug';
import styles from './UiScreen.module.css';

export interface UiScreenProps {
  scenario: string;
  route: string;
  theme?: 'light' | 'dark';
  chrome?: boolean;
  height?: number;
  caption?: string;
  alt: string;
}

// One probe per page load: does ui-mock/mock.html exist?
let probe: Promise<boolean> | null = null;
function mockAvailable(url: string): Promise<boolean> {
  probe ??= fetch(url)
    .then(async (r) => r.ok && (r.headers.get('content-type') ?? '').includes('html') && !(await r.text()).includes('__docusaurus'))
    .catch(() => false);
  return probe;
}

export default function UiScreen({
  scenario,
  route,
  theme,
  chrome = true,
  height = 560,
  caption,
  alt,
}: UiScreenProps): React.JSX.Element {
  const {colorMode} = useColorMode();
  const resolved = theme ?? colorMode;
  const slug = uiScreenSlug(scenario, route, resolved);
  const mockUrl = useBaseUrl('/ui-mock/mock.html');
  const imgUrl = useBaseUrl(`/img/ui/${slug}.png`);
  const [mounted, setMounted] = useState(false);
  const [ok, setOk] = useState<boolean | null>(null);
  const [active, setActive] = useState(false);
  const [imgFailed, setImgFailed] = useState(false);

  useEffect(() => {
    setMounted(true);
    let live = true;
    mockAvailable(mockUrl).then((v) => live && setOk(v));
    return () => {
      live = false;
    };
  }, [mockUrl]);

  useEffect(() => setImgFailed(false), [imgUrl]);

  const q = new URLSearchParams({scenario, route, theme: resolved, chrome: chrome ? '1' : '0'});
  const src = `${mockUrl}?${q.toString()}`;
  const showFrame = mounted && ok === true;
  const showFallback = mounted && ok === false;
  const dev = process.env.NODE_ENV !== 'production';

  return (
    <figure className={styles.figure}>
      <div className={styles.frame}>
        <div className={styles.bar}>
          <span className={styles.dots} aria-hidden="true">
            <i /><i /><i />
          </span>
          <span className={styles.url}>{route}</span>
        </div>
        <div className={styles.viewport} style={{height}}>
          {showFrame && (
            <>
              <iframe
                key={src}
                className={styles.iframe}
                src={src}
                title={alt}
                loading="lazy"
                style={{pointerEvents: active ? 'auto' : 'none'}}
              />
              {!active && (
                <button type="button" className={styles.overlay} onClick={() => setActive(true)}>
                  <span>Click to interact</span>
                </button>
              )}
            </>
          )}
          {showFallback && !imgFailed && (
            <img className={styles.img} src={imgUrl} alt={alt} onError={() => setImgFailed(true)} />
          )}
          {showFallback && imgFailed && (
            <div className={styles.placeholder} role="img" aria-label={alt}>
              <strong>{alt}</strong>
              {dev ? (
                <span>
                  UI mock not found. Run <code>npm run build:ui-mock</code> then reload.
                  <br />
                  <small>{slug}</small>
                </span>
              ) : (
                <span>Preview unavailable.</span>
              )}
            </div>
          )}
          {!mounted && <div className={styles.placeholder} />}
          <noscript>
            <img className={styles.img} src={imgUrl} alt={alt} />
          </noscript>
        </div>
      </div>
      {showFrame && (
        <a className={styles.fullscreen} href={src} target="_blank" rel="noopener noreferrer">
          Open full screen
        </a>
      )}
      {caption && <figcaption className={styles.caption}>{caption}</figcaption>}
    </figure>
  );
}
