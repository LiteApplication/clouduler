import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react';
import './helptip.css';

const GAP = 8;
const EDGE = 8;

/** A "?" button that explains a field. The text opens on hover, keyboard focus
 *  and tap; Esc or a tap elsewhere closes it. `configKey` is shown small and
 *  mono at the foot, so the raw config name stays findable. */
export function HelpTip({ configKey, children }: { label?: string; configKey?: string; children: ReactNode }) {
  const id = useId();
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLSpanElement>(null);
  const timer = useRef<number>();
  const [hover, setHover] = useState(false);
  const [focus, setFocus] = useState(false);
  const [pinned, setPinned] = useState(false);
  const [style, setStyle] = useState<CSSProperties>({});
  const open = hover || focus || pinned;

  const place = useCallback(() => {
    const b = btn.current?.getBoundingClientRect();
    const p = pop.current;
    if (!b || !p) return;
    const w = p.offsetWidth;
    const h = p.offsetHeight;
    const left = Math.min(Math.max(b.left + b.width / 2 - w / 2, EDGE), window.innerWidth - w - EDGE);
    const below = b.bottom + GAP;
    const top = below + h + EDGE > window.innerHeight && b.top - GAP - h > EDGE ? b.top - GAP - h : below;
    setStyle({ left, top });
  }, []);

  useLayoutEffect(() => {
    if (open) place();
  }, [open, place]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setHover(false);
        setPinned(false);
      }
    };
    const onDown = (e: Event) => {
      const t = e.target as Node;
      if (btn.current?.contains(t) || pop.current?.contains(t)) return;
      setHover(false);
      setPinned(false);
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('pointerdown', onDown);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.removeEventListener('pointerdown', onDown);
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
    };
  }, [open, place]);

  useEffect(() => () => window.clearTimeout(timer.current), []);

  const enter = () => {
    window.clearTimeout(timer.current);
    setHover(true);
  };
  // A short delay lets the pointer cross the gap into the text without closing it.
  const leave = () => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setHover(false), 120);
  };

  return (
    <span className="cl-tip" onMouseEnter={enter} onMouseLeave={leave}>
      <button
        ref={btn}
        type="button"
        className="cl-tip__btn"
        aria-label="More info"
        aria-describedby={id}
        aria-expanded={open}
        onClick={() => setPinned((v) => !v)}
        onFocus={() => setFocus(true)}
        onBlur={() => setFocus(false)}
      >
        ?
      </button>
      <span ref={pop} id={id} role="tooltip" className="cl-tip__pop" style={style} hidden={!open}>
        {children}
        {configKey && <code className="cl-tip__key">{configKey}</code>}
      </span>
    </span>
  );
}
