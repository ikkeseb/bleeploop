// Loaded only by plugin-probe.mjs's private Vite server, in the actual Tauri WebView: pick the test
// plugins through each slot's own source picker and read what the slot shows back; with Surge XT in the
// plugin folders (`window.__lfEditorProbe`), open its editors from slot A.
import { engineDevice } from '../../src/ui/state/engine-store';
import { platform } from '../../src/platform';

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const report = (kind, data) => console.error('[linux-plugin-probe]', JSON.stringify({ kind, ...data }));

async function until(what, read, timeout = 30000) {
  const end = performance.now() + timeout;
  do {
    const value = read();
    if (value) return value;
    await sleep(50);
  } while (performance.now() < end);
  throw new Error(`Timed out waiting for ${what}`);
}

const TEST_GAIN = { ids: ['dev.bleeploop.test-gain', '4c46546573744761696e50726f633031'], name: 'LF Test Gain' };
const SURGE = { ids: ['org.surge-synth-team.surge-xt', 'abcdef019182faeb566d624153675854'], name: 'Surge XT' };

async function pick(letter, format, plugin = TEST_GAIN) {
  const select = () => document.querySelector(`select[aria-label="Source for slot ${letter}"]`);
  const option = await until(`slot ${letter}'s ${format} ${plugin.name} in the picker`, () =>
    [...(select()?.options ?? [])].find((o) => {
      try {
        const [f, , id] = JSON.parse(o.value);
        return f === format && plugin.ids.includes(id);
      } catch {
        return false;
      }
    }));
  const el = select();
  el.value = option.value;
  el.dispatchEvent(new Event('change', { bubbles: true }));
  // The slot draws its plugin plate anew once the plugin is in, picker included: read the live one.
  // A slot that is not the active one folds to its name (`Show slot B: …`).
  const plate = () =>
    select()?.closest('.plate__pick')?.querySelector('.plate__name')?.textContent?.trim() ??
    document.querySelector(`[aria-label^="Show slot ${letter}: "]`)?.getAttribute('aria-label')?.replace(`Show slot ${letter}: `, '');
  await until(`slot ${letter} to show the plugin`, () => !select()?.disabled && plate() === plugin.name)
    .catch((e) => { throw new Error(`${e.message}: the plate reads ${JSON.stringify(plate())}, the picker ${select() ? (select().disabled ? 'disabled' : 'enabled') : 'gone'}, slot text ${JSON.stringify(document.querySelector(`[aria-label^="Slot ${letter}"]`)?.textContent?.slice(0, 200))}`); });
  const loaded = await platform.pluginHost.listLoaded();
  const mine = loaded.find((p) => p.slot === (letter === 'A' ? 0 : 1));
  if (!mine || mine.descriptor.format !== format) throw new Error(`slot ${letter} holds ${JSON.stringify(mine)}`);
  if (plugin !== TEST_GAIN) return;
  const params = await platform.pluginHost.listParams(letter === 'A' ? 0 : 1);
  const gain = params.find((p) => p.name === 'Gain');
  if (!gain || gain.id !== 7) throw new Error(`slot ${letter}'s parameters: ${JSON.stringify(params)}`);
  report('loaded', { slot: letter, format, label: option.text, plate: plate(), gain });
}

/** The slot's source to Off, and wait until its plugin has left the engine. */
async function off(letter) {
  const select = () => document.querySelector(`select[aria-label="Source for slot ${letter}"]`);
  await until(`slot ${letter}'s picker`, () => select() && !select().disabled);
  select().value = 'off';
  select().dispatchEvent(new Event('change', { bubbles: true }));
  const slot = letter === 'A' ? 0 : 1;
  const end = performance.now() + 15000;
  while ((await platform.pluginHost.listLoaded()).some((p) => p.slot === slot)) {
    if (performance.now() > end) throw new Error(`slot ${letter} did not unload`);
    await sleep(100);
  }
}

/** Slot A's editor control, and whether the slot shows its editor open. */
const editorButton = () => document.querySelector('button[aria-label="Editor for slot A"]');
const editorShown = () => editorButton()?.getAttribute('aria-pressed') === 'true';

/** An editor is open: tell the runner (which screenshots the display) and hold still while it does. */
async function editorOpened(format) {
  await until(`slot A's ${format} editor to open`, editorShown, 20000);
  report('editor', { slot: 'A', format, name: SURGE.name });
  await sleep(5000);
}

/** An editor closed: tell the runner (which checks its window is gone) and hold still meanwhile, so no
 * reopen can take the window's id first. */
async function editorClosed(format, by) {
  report('editor-closed', { slot: 'A', format, by });
  await sleep(3000);
}

/** Surge XT's editors from slot A: opened by the pick, then by the slot's editor control. */
async function surgeEditors() {
  await pick('A', 'clap', SURGE);
  await editorOpened('clap');
  editorButton().click();
  await until('slot A\'s editor to close by its control', () => editorButton() && !editorShown() && !editorButton().disabled, 10000);
  await editorClosed('clap', 'its control');
  editorButton().click();
  await editorOpened('clap');
  await off('A');
  await editorClosed('clap', 'the unload');
  await pick('A', 'vst3', SURGE);
  await editorOpened('vst3');
}

(async () => {
  try {
    await until('the JACK device', () => engineDevice()?.backend === 'Jack');
    report('device', { device: engineDevice() });
    await pick('A', 'clap');
    await pick('B', 'vst3');
    // Off in slot A: its live-off, then the plugin leaves the running engine and its owner ends.
    const select = () => document.querySelector('select[aria-label="Source for slot A"]');
    await until('slot A\'s picker', () => select() && !select().disabled);
    select().value = 'off';
    select().dispatchEvent(new Event('change', { bubbles: true }));
    const end = performance.now() + 15000;
    let left = null;
    while (performance.now() < end) {
      left = await platform.pluginHost.listLoaded();
      if (!left.some((p) => p.slot === 0)) break;
      await sleep(100);
    }
    if (left.some((p) => p.slot === 0)) throw new Error(`slot A still holds ${JSON.stringify(left)}`);
    report('unloaded', { slot: 'A', loaded: left.map((p) => `${p.slot}:${p.descriptor.format}`) });
    if (window.__lfEditorProbe) await surgeEditors();
    report('done', {});
  } catch (error) {
    report('failed', { error: String(error?.message ?? error) });
  }
})();
