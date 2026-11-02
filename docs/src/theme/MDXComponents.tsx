import React from 'react';
import MDXComponents from '@theme-original/MDXComponents';
import UiScreen from '@site/src/components/UiScreen';
import UiCli, {UiTab, CliTab} from '@site/src/components/UiCli';
import SetupForm from '@site/src/components/SetupForm';
import Cmd from '@site/src/components/Cmd';
import DistroTabs from '@site/src/components/DistroTabs';
import OptionTable from '@site/src/components/OptionTable';
import Quirk from '@site/src/components/Quirk';

export default {
  ...MDXComponents,
  UiScreen,
  UiCli,
  UiTab,
  CliTab,
  // The real provider lives in theme/Root.tsx; this passthrough keeps stray <SetupProvider> tags harmless.
  SetupProvider: ({children}: {children?: React.ReactNode}) => <>{children}</>,
  SetupForm,
  Cmd,
  DistroTabs,
  OptionTable,
  Quirk,
};
