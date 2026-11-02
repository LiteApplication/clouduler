import React from 'react';
import Tabs from '@theme/Tabs';
import TabItem from '@theme/TabItem';

type Slot = {children?: React.ReactNode};

/** Marker components: only their children are rendered, inside a tab. */
export function UiTab({children}: Slot): React.JSX.Element {
  return <>{children}</>;
}
export function CliTab({children}: Slot): React.JSX.Element {
  return <>{children}</>;
}

function UiCliRoot({children}: Slot): React.JSX.Element {
  const kids = React.Children.toArray(children);
  const find = (T: unknown) =>
    kids.filter((k): k is React.ReactElement<Slot> => React.isValidElement(k) && k.type === T);
  const ui = find(UiTab);
  const cli = find(CliTab);
  return (
    <Tabs groupId="ui-cli" queryString={false}>
      <TabItem value="ui" label="Web UI" default>
        {ui.map((k, i) => <React.Fragment key={i}>{k.props.children}</React.Fragment>)}
      </TabItem>
      <TabItem value="cli" label="CLI / config">
        {cli.map((k, i) => <React.Fragment key={i}>{k.props.children}</React.Fragment>)}
      </TabItem>
    </Tabs>
  );
}

const UiCli = Object.assign(UiCliRoot, {Ui: UiTab, Cli: CliTab});
export default UiCli;
