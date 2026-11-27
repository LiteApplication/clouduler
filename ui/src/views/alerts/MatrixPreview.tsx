// A picture of the Matrix message a destination would post, so the style
// choice can be judged without sending anything.

import { useState } from 'react';
import type { MatrixStyle } from '../../api/types';
import { Chip } from '../../ds';

type Sample = 'critical' | 'resolved';

const SAMPLES: Record<Sample, { emoji: string; word: string; title: string; detail: string; where: string; code?: string }> = {
  critical: {
    emoji: '\u{1F534}',
    word: 'CRITICAL',
    title: 'media-radarr-1 keeps crashing',
    detail: 'Killed 3 times in 20 minutes (exit 137). It used 498 of 512 MB.',
    where: 'stack media · instance media-radarr-1 · node atlas-02',
    code: 'FATAL out of memory: cannot allocate 268435456 bytes',
  },
  resolved: {
    emoji: '✅',
    word: 'RESOLVED',
    title: 'atlas-02 memory pressure',
    detail: 'Memory was high for 22 minutes, then dropped back to normal.',
    where: 'node atlas-02',
  },
};

export function MatrixPreview({ style }: { style: MatrixStyle }) {
  const [sample, setSample] = useState<Sample>('critical');
  const s = SAMPLES[sample];
  return (
    <div className="col" style={{ gap: 8 }}>
      <div className="chips" role="group" aria-label="Preview as">
        {(['critical', 'resolved'] as Sample[]).map((x) => (
          <Chip key={x} label={x[0].toUpperCase() + x.slice(1)} selected={sample === x} onClick={() => setSample(x)} />
        ))}
      </div>
      <div className="col" role="img" aria-label={`Preview of a ${sample} Matrix message`} style={{ gap: 4, padding: 12, borderRadius: 8, border: '1px solid var(--line)' }}>
        {style === 'short' ? (
          <span>
            {s.emoji} <b>{s.word}</b> {s.title} — <em>{s.where}</em>
          </span>
        ) : (
          <>
            <span>
              {s.emoji} <b>{s.word}</b> {s.title}
            </span>
            <blockquote className="col" style={{ gap: 4, margin: 0, paddingLeft: 10, borderLeft: '3px solid var(--line)' }}>
              <span>{s.detail}</span>
              <em>{s.where}{sample === 'critical' ? ' · x3' : ''}</em>
              {s.code && <pre className="code" style={{ margin: 0 }}>{s.code}</pre>}
            </blockquote>
            <span>Open in Clouduler</span>
          </>
        )}
      </div>
    </div>
  );
}
