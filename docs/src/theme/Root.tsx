import React from 'react';
import {SetupProvider} from '@site/src/components/SetupContext';

export default function Root({children}: {children: React.ReactNode}): React.JSX.Element {
  return <SetupProvider>{children}</SetupProvider>;
}
