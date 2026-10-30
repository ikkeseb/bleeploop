/**
 * Shared scaffolding for the browser probes in `verify/probes/`: the target URL, a headless Chromium,
 * the app page with `window.__lf` ready, page-error and `console.error` collection and the result line.
 *
 * A probe is a module that calls `probe(async (p) => { … })` once and throws (usually through
 * `node:assert`) on failure. `probe` prints `=== RESULT: <name> passed ===` or `… FAILED ===`, sets the
 * exit code and always closes the browser. It fails on any `console.error` line a page logged that the
 * probe did not consume (`AppPage.consumeConsoleErrors`), and on any uncaught page error. It empties the
 * probe's own folder `logs/<name>/` before the probe runs, so a screenshot an earlier run left there
 * never passes for this run's. `pnpm probe` (`verify/run-probes.mjs`) starts its own Vite and passes
 * `--url`; run directly, a probe targets http://localhost:1420.
 */
import assert from 'node:assert/strict';
import { readdir, rm } from 'node:fs/promises';
import { basename, join } from 'node:path';
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
  /** `console.error` texts not consumed yet. `probe` fails on any left when the probe ends. */
  consoleErrors: string[];
  /**
   * Takes the `console.error` texts logged so far that match `pattern` (a string matches as a substring)
   * out of `consoleErrors` and returns them, for the probe to assert on: how a probe expects an error.
   * One logged after the call stays, and fails the probe unless a later call takes it.
   */
  consumeConsoleErrors(pattern: RegExp | string): string[];
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
  /** Replaces the default launch options (audio without a user gesture). Every launch is headless, renders
   *  in software and is muted whatever this says: the browser tier plays nothing, and a probe never
   *  touches the GPU (`MACHINE_SAFE_ARGS`). */
  launch?: LaunchOptions;
}

const DEFAULT_LAUNCH: LaunchOptions = { args: ['--autoplay-policy=no-user-gesture-required'] };

/** Added to every launch: Chromium renders on the CPU, never the real GPU (parallel real-GPU Chromium
 * under WSL is the one lead on a host hard reboot), and plays no sound. `--disable-gpu`, not the
 * SwiftShader GL flags WebGL repos use: the app draws no WebGL, and Windows Chromium rasterising Canvas
 * and SVG through ANGLE SwiftShader moved the pixel probes off what CI's Linux renders (the gear's
 * symmetry, the overdub's amber). Measured over CDP `SystemInfo.getInfo` on Windows Chromium: every
 * feature reads software or off, and the one GPU device is SwiftShader. */
const MACHINE_SAFE_ARGS = ['--disable-gpu', '--mute-audio'];

/** Empties `dir` when it exists, keeping the folder: a probe may have created it already. */
async function empty(dir: string): Promise<void> {
  const entries = await readdir(dir).catch(() => []);
  await Promise.all(entries.map((entry) => rm(join(dir, entry), { recursive: true, force: true })));
}

export async function probe(body: (p: Probe) => Promise<unknown>, options: ProbeOptions = {}): Promise<void> {
  const name = basename(process.argv[1] ?? 'probe', '.mjs');
  const opened: { app: AppPage; allowPageErrors: boolean }[] = [];
  let browser: Browser | undefined;
  let ran = false;
  try {
    await empty(join('logs', name));
    const launch = options.launch ?? DEFAULT_LAUNCH;
    browser = await chromium.launch({ ...launch, headless: true, args: [...(launch.args ?? []), ...MACHINE_SAFE_ARGS] });
    const b = browser;
    const open = async (o: OpenOptions = {}): Promise<AppPage> => {
      const page = o.context ? await o.context.newPage() : await b.newPage(o.viewport ? { viewport: o.viewport } : {});
      if (o.context && o.viewport) await page.setViewportSize(o.viewport);
      const consoleErrors: string[] = [];
      const consumeConsoleErrors = (pattern: RegExp | string): string[] => {
        const taken: string[] = [];
        const rest: string[] = [];
        for (const text of consoleErrors) {
          const hit = typeof pattern === 'string' ? text.includes(pattern) : text.search(pattern) >= 0;
          (hit ? taken : rest).push(text);
        }
        consoleErrors.splice(0, consoleErrors.length, ...rest);
        return taken;
      };
      const app: AppPage = { page, pageErrors: [], consoleErrors, consumeConsoleErrors };
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
    ran = true;
    for (const [i, { app, allowPageErrors }] of opened.entries()) {
      if (!allowPageErrors) assert.deepEqual(app.pageErrors, [], `unexpected uncaught page errors (page ${i + 1})`);
      assert.deepEqual(app.consoleErrors, [], `console.error lines the probe did not consume (page ${i + 1})`);
    }
    console.log(`=== RESULT: ${name} passed ===`);
  } catch (error) {
    console.error(error);
    // A failed assert in the body skips the checks above: what the pages logged is evidence.
    for (const [i, { app }] of opened.entries()) {
      if (!ran && app.consoleErrors.length) console.error(`console.error lines on page ${i + 1}:`, app.consoleErrors);
    }
    console.log(`=== RESULT: ${name} FAILED ===`);
    process.exitCode = 1;
  } finally {
    await browser?.close();
  }
}
