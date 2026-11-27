// Loaded only by plugin-probe.mjs's private Vite server, in the actual Tauri WebView: pick the test
// plugins through each slot's own source picker and read what the slot shows back.
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

async function pick(letter, format) {
  const select = () => document.querySelector(`select[aria-label="Source for slot ${letter}"]`);
  const option = await until(`slot ${letter}'s ${format} test plugin in the picker`, () =>
    [...(select()?.options ?? [])].find((o) => {
      try {
        const [f, , id] = JSON.parse(o.value);
        return f === format && (id === 'dev.bleeploop.test-gain' || id === '4c46546573744761696e50726f633031');
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
  await until(`slot ${letter} to show the plugin`, () => !select()?.disabled && plate() === 'LF Test Gain')
    .catch((e) => { throw new Error(`${e.message}: the plate reads ${JSON.stringify(plate())}, the picker ${select() ? (select().disabled ? 'disabled' : 'enabled') : 'gone'}, slot text ${JSON.stringify(document.querySelector(`[aria-label^="Slot ${letter}"]`)?.textContent?.slice(0, 200))}`); });
  const loaded = await platform.pluginHost.listLoaded();
  const mine = loaded.find((p) => p.slot === (letter === 'A' ? 0 : 1));
  if (!mine || mine.descriptor.format !== format) throw new Error(`slot ${letter} holds ${JSON.stringify(mine)}`);
  const params = await platform.pluginHost.listParams(letter === 'A' ? 0 : 1);
  const gain = params.find((p) => p.name === 'Gain');
  if (!gain || gain.id !== 7) throw new Error(`slot ${letter}'s parameters: ${JSON.stringify(params)}`);
  report('loaded', { slot: letter, format, label: option.text, plate: plate(), gain });
}

(async () => {
  try {
    await until('the JACK device', () => engineDevice()?.backend === 'Jack');
    report('device', { device: engineDevice() });
    await pick('A', 'clap');
    await pick('B', 'vst3');
    report('done', {});
  } catch (error) {
    report('failed', { error: String(error?.message ?? error) });
  }
})();
