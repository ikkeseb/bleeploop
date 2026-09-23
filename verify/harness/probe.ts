/**
 * Shared scaffolding for the browser probes in `verify/probes/`: the target URL, a headless Chromium,
 * the app page with `window.__lf` ready, page-error collection and the result line.
 *
 * A probe is a module that calls `probe(async (p) => { … })` once and throws (usually through
 * `node:assert`) on failure. `probe` prints `=== RESULT: <name> passed ===` or `… FAILED ===`, sets the
 * exit code and always closes the browser. `pnpm probe` (`verify/run-probes.mjs`) starts its own Vite
 * and passes `--url`; run directly, a probe targets http://localhost:1420.
 */
import assert from 'node:assert/strict';
import { basename } from 'node:path';
import { chromium, type Browser, type BrowserContext, type LaunchOptions, type Page } from 'playwright';

/** The value of `--name=value` on the command line, or undefined. */
export function arg(name: string): string | undefined {
  const prefix = `--${name}=`;
  return process.argv.find((a) => a.startsWith(prefix))?.slice(prefix.length);
}

/** Whether the bare flag `--name` is on the command line. */
export function flag(name: string): boolean {
  return process.argv.includes(`--${name}`);
}

export const url = arg('url') ?? 'http://localhost:1420';

export interface AppPage {
  page: Page;
  /** Uncaught page errors. `probe` fails on any, unless the page was opened with `allowPageErrors`. */
  pageErrors: string[];
  /** `console.error` texts, for the probe to assert on. */
  consoleErrors: string[];
}

export interface OpenOptions {
  /** Open in this context (a fresh profile, downloads) instead of the browser's default one. */
  context?: BrowserContext;
  viewport?: { width: number; height: number };
  /** Runs before navigation: `addInitScript`, routes, CDP sessions. */
  init?: (page: Page) => unknown;
  /** Do not wait for `window.__lf` after navigation. */
  noLf?: boolean;
  /** The probe asserts on `pageErrors` itself (it injects failures that surface as uncaught). */
  allowPageErrors?: boolean;
}

export interface Probe {
  browser: Browser;
  url: string;
  /** Open the app in a new page and wait for `window.__lf`. */
  open(options?: OpenOptions): Promise<AppPage>;
}

export interface ProbeOptions {
  /** Replaces the default launch options (headless, audio without a user gesture). */
  launch?: LaunchOptions;
}

const DEFAULT_LAUNCH: LaunchOptions = { headless: true, args: ['--autoplay-policy=no-user-gesture-required'] };

export async function probe(body: (p: Probe) => Promise<unknown>, options: ProbeOptions = {}): Promise<void> {
  const name = basename(process.argv[1] ?? 'probe', '.mjs');
  const opened: { app: AppPage; allowPageErrors: boolean }[] = [];
  let browser: Browser | undefined;
  try {
    browser = await chromium.launch(options.launch ?? DEFAULT_LAUNCH);
    const b = browser;
    const open = async (o: OpenOptions = {}): Promise<AppPage> => {
      const page = o.context ? await o.context.newPage() : await b.newPage(o.viewport ? { viewport: o.viewport } : {});
      if (o.context && o.viewport) await page.setViewportSize(o.viewport);
      const app: AppPage = { page, pageErrors: [], consoleErrors: [] };
      page.on('pageerror', (error) => app.pageErrors.push(String(error)));
      page.on('console', (msg) => {
        if (msg.type() === 'error') app.consoleErrors.push(msg.text());
      });
      opened.push({ app, allowPageErrors: !!o.allowPageErrors });
      await o.init?.(page);
      await page.goto(url);
      if (!o.noLf) await page.waitForFunction(() => '__lf' in window, undefined, { timeout: 30_000 });
      return app;
    };
    await body({ browser: b, url, open });
    for (const { app, allowPageErrors } of opened) {
      if (!allowPageErrors) assert.deepEqual(app.pageErrors, [], 'unexpected uncaught page errors');
    }
    console.log(`=== RESULT: ${name} passed ===`);
  } catch (error) {
    console.error(error);
    console.log(`=== RESULT: ${name} FAILED ===`);
    process.exitCode = 1;
  } finally {
    await browser?.close();
  }
}
