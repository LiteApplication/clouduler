/**
 * Generates static/img/ui/<slug>.png fallbacks for every <UiScreen> in content/**.mdx,
 * by loading static/ui-mock/mock.html in headless chromium (light + dark).
 *
 *   npm run screenshots [-- --force]
 */
import {createServer, type Server} from 'node:http';
import {readdirSync, readFileSync, statSync, existsSync, mkdirSync} from 'node:fs';
import {join, extname, resolve, dirname, relative} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium, type Browser} from 'playwright';
import {uiScreenSlug} from '../src/components/uiScreenSlug';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CONTENT = join(ROOT, 'content');
const STATIC = join(ROOT, 'static');
const OUT = join(STATIC, 'img', 'ui');
const MOCK = join(STATIC, 'ui-mock', 'mock.html');
const WIDTH = 1400;
const DEFAULT_HEIGHT = 560;
const CONCURRENCY = 4;
const SETTLE_MS = 1500;
const force = process.argv.includes('--force');

interface Shot {
  scenario: string;
  route: string;
  chrome: boolean;
  height: number;
  theme: 'light' | 'dark';
  slug: string;
  sources: string[];
}

function walk(dir: string): string[] {
  return readdirSync(dir, {withFileTypes: true}).flatMap((e) =>
    e.isDirectory() ? walk(join(dir, e.name)) : e.name.endsWith('.mdx') ? [join(dir, e.name)] : [],
  );
}

function parseProps(src: string): Record<string, string> {
  const props: Record<string, string> = {};
  const re = /([A-Za-z_][\w-]*)\s*=\s*(?:"([^"]*)"|'([^']*)'|\{\s*(?:"([^"]*)"|'([^']*)'|`([^`]*)`|([^}]*?))\s*\})/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(src))) props[m[1]] = (m[2] ?? m[3] ?? m[4] ?? m[5] ?? m[6] ?? m[7] ?? '').trim();
  return props;
}

function collect(): Shot[] {
  const map = new Map<string, Shot>();
  for (const file of walk(CONTENT)) {
    // Drop fenced code blocks so documented examples are not captured.
    const text = readFileSync(file, 'utf8').replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, '').replace(/`[^`\n]*`/g, '');
    for (const m of text.matchAll(/<UiScreen\b([\s\S]*?)\/>/g)) {
      const p = parseProps(m[1]);
      if (!p.scenario || !p.route) {
        console.warn(`warn: ${relative(ROOT, file)}: <UiScreen> without scenario/route skipped`);
        continue;
      }
      const chrome = p.chrome !== 'false' && p.chrome !== '0';
      const height = Number(p.height) || DEFAULT_HEIGHT;
      for (const theme of ['light', 'dark'] as const) {
        const slug = uiScreenSlug(p.scenario, p.route, theme);
        // Same slug but different chrome/height would overwrite; keep the first and note it.
        const key = `${slug}|${chrome}|${height}`;
        const prev = [...map.values()].find((s) => s.slug === slug && (s.chrome !== chrome || s.height !== height));
        if (prev && !map.has(key)) {
          console.warn(`warn: ${slug} used with different chrome/height; keeping ${prev.chrome}/${prev.height}`);
          prev.sources.push(file);
          continue;
        }
        const s = map.get(key) ?? {scenario: p.scenario, route: p.route, chrome, height, theme, slug, sources: []};
        s.sources.push(file);
        map.set(key, s);
      }
    }
  }
  return [...map.values()];
}

const MIME: Record<string, string> = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css',
  '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png', '.woff2': 'font/woff2',
  '.woff': 'font/woff', '.ico': 'image/x-icon', '.webmanifest': 'application/manifest+json',
};

function serve(): Promise<{server: Server; port: number}> {
  const server = createServer((req, res) => {
    const path = decodeURIComponent((req.url ?? '/').split('?')[0]);
    const file = resolve(STATIC, '.' + path);
    if (!file.startsWith(STATIC) || !existsSync(file) || statSync(file).isDirectory()) {
      res.writeHead(404).end('not found');
      return;
    }
    res.writeHead(200, {'content-type': MIME[extname(file)] ?? 'application/octet-stream'});
    res.end(readFileSync(file));
  });
  return new Promise((ok) => server.listen(0, '127.0.0.1', () => ok({server, port: (server.address() as {port: number}).port})));
}

async function main(): Promise<void> {
  if (!existsSync(MOCK)) {
    console.error(`screenshots: ${relative(ROOT, MOCK)} not found. Run \`npm run build:ui-mock\` first.`);
    process.exit(1);
  }
  const shots = collect();
  if (shots.length === 0) {
    console.log('screenshots: no <UiScreen> usages found');
    return;
  }
  mkdirSync(OUT, {recursive: true});
  const mockTime = statSync(MOCK).mtimeMs;
  const todo = shots.filter((s) => {
    if (force) return true;
    const png = join(OUT, `${s.slug}.png`);
    if (!existsSync(png)) return true;
    const t = statSync(png).mtimeMs;
    return t < mockTime || s.sources.some((f) => statSync(f).mtimeMs > t);
  });
  console.log(`screenshots: ${shots.length} total, ${todo.length} to render, ${shots.length - todo.length} up to date`);
  if (todo.length === 0) return;

  const {server, port} = await serve();
  let browser: Browser;
  try {
    browser = await chromium.launch();
  } catch (e) {
    server.close();
    console.error(`screenshots: cannot launch chromium (${(e as Error).message.split('\n')[0]}).\nRun \`npm run setup:browsers\`.`);
    process.exit(1);
  }
  const failures: string[] = [];
  const queue = [...todo];

  async function worker(): Promise<void> {
    for (let s = queue.shift(); s; s = queue.shift()) {
      const ctx = await browser.newContext({
        viewport: {width: WIDTH, height: s.height},
        colorScheme: s.theme,
        reducedMotion: 'reduce',
      });
      const page = await ctx.newPage();
      const problems: string[] = [];
      page.on('console', (m) => {
        if (m.type() === 'error' && m.text().includes('[mock]')) problems.push(`console: ${m.text()}`);
      });
      page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
      const q = new URLSearchParams({scenario: s.scenario, route: s.route, theme: s.theme, chrome: s.chrome ? '1' : '0'});
      try {
        await page.goto(`http://127.0.0.1:${port}/ui-mock/mock.html?${q}`, {waitUntil: 'load'});
        // The mock keeps an SSE stream open, so networkidle may never fire: cap it, then settle.
        await page.waitForLoadState('networkidle', {timeout: 4000}).catch(() => {});
        await page.waitForSelector('#root > *', {timeout: 10000});
        await page.waitForTimeout(SETTLE_MS);
        await page.screenshot({path: join(OUT, `${s.slug}.png`)});
      } catch (e) {
        problems.push(`error: ${(e as Error).message.split('\n')[0]}`);
      }
      if (problems.length) failures.push(`${s.slug}\n    ${[...new Set(problems)].join('\n    ')}`);
      else console.log(`  ok ${s.slug}`);
      await ctx.close();
    }
  }

  await Promise.all(Array.from({length: Math.min(CONCURRENCY, todo.length)}, worker));
  await browser.close();
  server.close();
  if (failures.length) {
    console.error(`\nscreenshots: ${failures.length} failure(s):\n  ${failures.join('\n  ')}`);
    process.exit(1);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
