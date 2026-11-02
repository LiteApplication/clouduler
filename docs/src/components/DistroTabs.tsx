import React from 'react';
import {useSetup} from './SetupContext';

type Slot = {children?: React.ReactNode};
export function ArchTab({children}: Slot): React.JSX.Element { return <>{children}</>; }
export function DebianTab({children}: Slot): React.JSX.Element { return <>{children}</>; }
export function RaspbianTab({children}: Slot): React.JSX.Element { return <>{children}</>; }

const DEFS = [
  {value: 'arch', label: 'Arch Linux', T: ArchTab},
  {value: 'debian', label: 'Debian', T: DebianTab},
  {value: 'raspbian', label: 'Raspberry Pi OS', T: RaspbianTab},
] as const;

function DistroTabsRoot({children}: Slot): React.JSX.Element {
  const kids = React.Children.toArray(children);
  const {values, set} = useSetup();
  // Shares the `distro` value with SetupForm (and so with every other DistroTabs and Cmd).
  const current = DEFS.some((d) => d.value === values.distro) ? values.distro : DEFS[0].value;
  const def = DEFS.find((d) => d.value === current)!;
  const mine = kids.filter((k): k is React.ReactElement<Slot> => React.isValidElement(k) && k.type === def.T);
  return (
    <div className="tabs-container">
      <ul role="tablist" aria-orientation="horizontal" className="tabs">
        {DEFS.map(({value, label}) => (
          <li
            key={value}
            role="tab"
            tabIndex={value === current ? 0 : -1}
            aria-selected={value === current}
            className={'tabs__item' + (value === current ? ' tabs__item--active' : '')}
            onClick={() => set('distro', value)}
            onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && set('distro', value)}
          >
            {label}
          </li>
        ))}
      </ul>
      <div className="margin-top--md" role="tabpanel">
        {mine.map((k, i) => <React.Fragment key={i}>{k.props.children}</React.Fragment>)}
      </div>
    </div>
  );
}

const DistroTabs = Object.assign(DistroTabsRoot, {Arch: ArchTab, Debian: DebianTab, Raspbian: RaspbianTab});
export default DistroTabs;
