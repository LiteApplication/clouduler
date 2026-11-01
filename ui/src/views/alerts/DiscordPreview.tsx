// A picture of the Discord message a destination would post, so the style,
// ping and button choices can be judged without sending anything.

import { useState } from 'react';
import type { DiscordNotify, DiscordNotifyLevels, DiscordStyle } from '../../api/types';
import { Chip, Icon } from '../../ds';

type Sample = 'critical' | 'warning' | 'resolved';

const SAMPLES: Record<Sample, { color: string; emoji: string; word: string; title: string; detail: string; code?: string }> = {
  critical: {
    color: '#e5484d',
    emoji: '\u{1F534}',
    word: 'CRITICAL',
    title: 'media-radarr-1 keeps crashing',
    detail: 'Killed 3 times in 20 minutes (exit 137). It used 498 of 512 MB.',
    code: 'FATAL out of memory: cannot allocate 268435456 bytes',
  },
  warning: {
    color: '#f5a623',
    emoji: '\u{1F7E0}',
    word: 'WARNING',
    title: 'atlas-02 media drive is 93% full',
    detail: '140 GB free in /srv/media. This turns critical below 5% free.',
  },
  resolved: {
    color: '#2fd0a8',
    emoji: '✅',
    word: 'RESOLVED',
    title: 'Resolved: atlas-02 memory pressure',
    detail: 'Memory was high for 22 minutes, then dropped back to normal.',
  },
};

export function DiscordPreview({
  style,
  mention,
  notify,
  buttons,
  username,
  cluster = 'homelab',
}: {
  style: DiscordStyle;
  mention: string;
  notify: DiscordNotifyLevels;
  buttons: boolean;
  username: string;
  cluster?: string;
}) {
  const [sample, setSample] = useState<Sample>('critical');
  const s = SAMPLES[sample];
  const level: DiscordNotify = notify[sample];
  const ping = level === 'ping' && mention.trim() ? mention.trim() : '';
  const name = username.trim() || 'Clouduler';
  return (
    <div className="col" style={{ gap: 8 }}>
      <div className="chips" role="group" aria-label="Preview as">
        {(['critical', 'warning', 'resolved'] as Sample[]).map((x) => (
          <Chip key={x} label={x[0].toUpperCase() + x.slice(1)} selected={sample === x} onClick={() => setSample(x)} />
        ))}
      </div>
      <div className="dc" role="img" aria-label={`Preview of a ${sample} Discord message`}>
        <div className="dc__av">
          <img src="/icon-192.png" alt="" />
        </div>
        <div className="dc__body">
          <div className="dc__head">
            <span className="dc__name">{name}</span>
            <span className="dc__bot">App</span>
            <span className="dc__time">Today at 09:12</span>
            {level === 'silent' && (
              <span className="dc__silent" title="Silent: no push notification">
                <Icon name="eye-off" size={12} /> silent
              </span>
            )}
          </div>
          {style === 'embed' ? (
            <>
              {ping && <span><span className="dc__ping">{ping}</span></span>}
              <div className="dc__embed" style={{ ['--dc-color' as string]: s.color }}>
                <div className="dc__embed-in">
                  <span className="dc__title">{s.title}</span>
                  <span className="dc__desc">{s.detail}</span>
                  {s.code && <pre className="dc__code">{s.code}</pre>}
                  <div className="dc__fields">
                    <span><b>Node</b>atlas-02</span>
                    <span><b>Stack</b>media</span>
                    <span><b>Occurrences</b>3</span>
                  </div>
                  <span className="dc__foot">{cluster} · instance.oom_killed</span>
                </div>
              </div>
            </>
          ) : (
            <span>
              {ping && <><span className="dc__ping">{ping}</span>{' '}</>}
              {s.emoji} <b>{s.word}</b> {s.title} — {s.detail}
            </span>
          )}
          {buttons && (
            <div className="dc__btns">
              <span className="dc__btn">Open in Clouduler <Icon name="external" size={12} /></span>
              {sample === 'critical' && <span className="dc__btn">Logs <Icon name="external" size={12} /></span>}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
