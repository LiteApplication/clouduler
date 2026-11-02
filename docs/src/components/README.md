# Docs components (global in MDX, no imports needed)

All are registered in `src/theme/MDXComponents.tsx`. Page files live in `docs/content/**` (`.mdx`).

## UiScreen
Props: `scenario: string`, `route: string`, `alt: string` (required); `theme?: 'light'|'dark'` (default: follows site colour mode), `chrome?: boolean` (default true), `height?: number` px (default 560), `caption?: string`.
Renders a browser frame around a lazy iframe of `ui-mock/mock.html?scenario=&route=&theme=&chrome=`, with a "Click to interact" overlay. Falls back to `img/ui/<slug>.png` (`uiScreenSlug(scenario, route, theme)` in `uiScreenSlug.ts`: `${scenario}--${route lowercased, non-alnum runs -> '-', trimmed, '/' -> 'root'}--${theme}`), or a placeholder.

    <UiScreen scenario="lan-3-nodes" route="/stacks" caption="Stacks" alt="Stacks view" />

## UiCli
Tabs "Web UI" | "CLI / config", synced site-wide (groupId `ui-cli`). Leave blank lines around markdown inside the tags. `<UiTab>` / `<CliTab>` are aliases of `UiCli.Ui` / `UiCli.Cli`.

    <UiCli>
    <UiCli.Ui>

    Open **Stacks**.

    </UiCli.Ui>
    <UiCli.Cli>

    (a fenced bash block here)

    </UiCli.Cli>
    </UiCli>

## SetupForm and Cmd
`SetupForm` props: `fields: {name, label, default?, help?, type?: 'text'|'select', options?: (string | {value,label})[]}[]`, `title?: string`. Values persist in localStorage (`clouduler-docs.setup.<name>`) and are shared across pages.
The field named `distro` (select with values `arch`, `debian`, `raspbian`) also drives `DistroTabs` and line filtering in `Cmd`.
`Cmd` props: `lang?: string` (default `bash`), `title?: string`; child must be a template-literal string. `{{var}}` is substituted (left literal, with a warning strip, if unset). Lines starting `[arch] ` or `[debian,raspbian] ` are shown only for that distro (all lines shown, prefix kept, if distro is unset).

    <SetupForm fields={[{name: 'ip', label: 'Node IP', default: '192.168.1.10'},
      {name: 'distro', label: 'Distribution', type: 'select', default: 'debian',
       options: [{value: 'arch', label: 'Arch Linux'}, {value: 'debian', label: 'Debian'}, {value: 'raspbian', label: 'Raspberry Pi OS'}]}]} />

    <Cmd title="Install">{`ssh root@{{ip}}
    [arch] pacman -S docker
    [debian,raspbian] apt install docker.io`}</Cmd>

## DistroTabs
Tabs Arch Linux / Debian / Raspberry Pi OS sharing the `distro` value with SetupForm. Children: `<DistroTabs.Arch>`, `<DistroTabs.Debian>`, `<DistroTabs.Raspbian>`. Blank lines around markdown.

## OptionTable
Props: `rows: {key: string, type: string, default?: string, reload: 'live'|'restart'|'n/a', required?: boolean, description: ReactNode, quirks?: ReactNode}[]`. Each key is a mono anchor `#opt-<key slugified>`; a filter box appears above 8 rows (it searches key/type/default and string descriptions).

    <OptionTable rows={[{key: 'proxy.enabled', type: 'bool', default: 'true', reload: 'live', description: 'Turns the proxy on.'}]} />

## Quirk
Props: `title: string`, `severity?: 'info'|'warn'|'danger'` (default warn); children are markdown.

    <Quirk title="Renaming a network" severity="danger">Use the rename endpoint, not an edit.</Quirk>

## Var
Props: `name: string` (a SetupForm field name), `fallback?: string` (shown when the field has no value yet; defaults to `{{name}}`), `plain?: boolean` (render a plain span instead of `<code>`).
Inline value from the shared SetupForm state, for prose and tables where `Cmd` cannot be used. Unset values are shown highlighted.

    The node is <Var name="hostname" fallback="atlas" />.
