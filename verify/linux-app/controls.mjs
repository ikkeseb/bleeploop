// Loaded only by the probe's private Vite server, in the actual Tauri WebView.
// Every gesture addresses this document. The native feed supplies the measured device and loop length.
import { confirmNativeClose } from '../../src/platform';
import { engineDevice, engineResets, onEngineEvent } from '../../src/ui/state/engine-store';
import { clock, looper } from '../../src/ui/state/audio';

const config = window.__linuxAppProbe;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const report = (kind, data) => console.error('[linux-app-probe]', JSON.stringify({ kind, ...data }));
const label = (name) => document.querySelector(`[aria-label="${name}"]`);
const lane = () => document.querySelector('.lp-lane')?.dataset.state;
const settings = () => document.querySelector('button[aria-label="Audio Settings"]');
const check = (ok, msg) => { if (!ok) throw new Error(msg); };

async function until(what, read, timeout = 5000) {
  const end = performance.now() + timeout;
  do {
    const value = read();
    if (value) return value;
    await sleep(15);
  } while (performance.now() < end);
  throw new Error(`Timed out waiting for ${what} (lane=${lane()}, device=${JSON.stringify(engineDevice())})`);
}

function select(name, value) {
  const element = label(name);
  check(element instanceof HTMLSelectElement && !element.disabled && element.getClientRects().length, `Unavailable: ${name}`);
  const option = [...element.options].find((o) => o.value === value || o.text.trim() === value);
  check(option, `${name} lacks "${value}": ${[...element.options].map((o) => o.text)}`);
  element.value = option.value;
  element.dispatchEvent(new Event('change', { bubbles: true }));
  return option.value;
}

async function toggle(name, on) {
  const button = label(name);
  check(button && button.getClientRects().length, `Missing ${name}`);
  const pressed = () => button.getAttribute('aria-pressed') === String(on);
  if (!pressed()) button.click();
  await until(name, pressed);
}

async function configure() {
  const first = engineDevice();
  check(first.backend === 'Jack' && first.inputOpen, `Unexpected device ${JSON.stringify(first)}`);
  const current = String(first.block);
  const selectors = ['Audio input device', 'Output device', 'Buffer size in frames'];
  const values = () => selectors.map((name) => label(name)?.value);
  const saved = () => window.__lf.audioDeviceSettings.readAudioDeviceSettings();
  const settingsReady = () => label('Audio input device')?.options.length > 1 && label('Output device')?.options.length > 1;
  await until('the JACK route choices', settingsReady);
  if (config.phase === 'choose') {
    check(values()[2] === '', 'Fresh profile does not follow the current server quantum');
    let previous = engineDevice();
    const input = select(selectors[0], config.input);
    await until('input choice applied', () => engineDevice() !== previous && saved().inputDeviceId === input && engineDevice()?.inputName === config.input);
    previous = engineDevice();
    const output = select(selectors[1], config.output);
    await until('output choice applied', () => engineDevice() !== previous && saved().outputDeviceId === output && engineDevice()?.outputName === config.output);
    // Pick only the quantum that is already running. The physical session remains at its own size.
    check(String(engineDevice().block) === current, 'Graph quantum changed during port selection');
    if ([...label(selectors[2]).options].some((option) => option.value === current)) {
      previous = engineDevice();
      select(selectors[2], current);
      await until('explicit current quantum saved', () => engineDevice() !== previous && saved().bufferFrames === first.block && values()[2] === current);
    } else {
      check(values()[2] === '' && saved().bufferFrames === null, 'Expected the server-default buffer preference');
    }
    return { picks: values(), device: engineDevice() };
  }
  check(JSON.stringify(values()) === JSON.stringify(config.picks), `Reopened controls show ${values()}, expected ${config.picks}`);
  check(first.inputName === config.input && first.outputName === config.output && first.block === config.expectedBlock, 'Reopened JACK route or actual quantum differs');
  return { picks: values(), device: first };
}

async function take(device) {
  settings().click();
  const lf = window.__lf;
  check(lf.native === null && lf.platform.kind === 'tauri', 'Expected native engine');
  check(label('Master volume')?.getAttribute('aria-valuenow') === '2' && clock.clickVolume() === 0, 'Quiet startup settings missing');
  check(!looper.inputArmed() && lane() === 'empty', 'Take must start empty, with no live input');
  select('Source for slot A', 'organ');
  await until('Organ selected', () => lf.slotIds()[0] === 'organ' && lf.activeSlot() === 0);
  await toggle('On-screen keyboard', true);
  await until('the keyboard', () => document.querySelector('.kb__keys')?.getClientRects().length);
  await toggle('Metronome click', false);
  await toggle('Fixed take length', true);
  const fixed = label('Fixed take length');
  while (looper.fixedLengthBars() > 1) {
    const before = looper.fixedLengthBars();
    fixed.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    await until('one less bar', () => looper.fixedLengthBars() < before);
  }
  check(clock.meter().beats === 4 && clock.bpm() >= 100 && clock.bpm() <= 150, 'Unexpected fresh-profile tempo or meter');
  document.activeElement?.blur();
  const expected = Math.round(device.sampleRate * 240 / clock.bpm());
  const barMs = expected / device.sampleRate * 1000;
  const timeline = {};
  const mark = (name) => { timeline[name] = Math.round(performance.now()); };
  const key = (k, down) => window.dispatchEvent(new KeyboardEvent(down ? 'keydown' : 'keyup', { key: k, code: 'Key' + k.toUpperCase(), bubbles: true }));
  const notes = [];
  async function note(k, ms) {
    key(k, true);
    try {
      const lit = await until('keyboard note down', () => document.querySelector('.kb__key--down'));
      notes.push(Number(lit.dataset.note));
      await sleep(ms);
    } finally { key(k, false); }
    await until('keyboard note released', () => !document.querySelector('.kb__key--down'));
  }
  async function counters() {
    const status = await lf.platform.engine.status();
    check(status?.backend === 'Jack' && status.block === device.block && status.sampleRate === device.sampleRate,
      'The measured JACK device changed');
    return { xruns: status.xruns, missingFrames: status.missingFrames };
  }
  // Allow WebKit startup, device linking and the independent recorder to settle before counting.
  await sleep(1200);
  const before = await counters();
  const feed = { resetsBefore: engineResets(), captureGapEvents: [], laneTransitions: [] };
  const unsubscribe = onEngineEvent((event) => {
    if (event.type === 'TakeRejected' || event.type === 'PassDropped') feed.captureGapEvents.push(event);
    if (event.type === 'Lane' && event.lane === 0) {
      const state = event.info.armed ? 'Armed' : event.info.state;
      if (feed.laneTransitions.at(-1)?.state !== state) feed.laneTransitions.push({ state, frame: event.frame });
    }
  });
  try {
    label('Track 1 record').click();
    await until('count-in', () => lane() === 'armed'); mark('armed');
    await until('record', () => lane() === 'rec'); mark('record');
    await sleep(170);
    await note('a', 125);
    await sleep(240);
    await note('g', 190);
    await until('play', () => lane() === 'play'); mark('play');
    check(looper.masterLengthFrames() === expected, 'The recorded loop is not exactly one bar');
    await sleep(2 * barMs - 70);
    label('Stop all tracks').click();
    await until('stop', () => lane() === 'stop'); mark('stop');
    await sleep(barMs + 350);
    await toggle('Mute master', true);
    label('Track 1 play').click();
    await until('manual play', () => lane() === 'play'); mark('manualPlay');
    label('Track 1 stop').click();
    await until('manual stop', () => lane() === 'stop'); mark('manualStop');
    return { frames: expected, notes, bpm: clock.bpm(), timeline, feed };
  } finally {
    feed.resetsAfter = engineResets();
    unsubscribe();
    key('a', false); key('g', false);
    lf.master.setMuted(true);
    report('timing', { before, after: await counters(), feed });
  }
}

async function run() {
  await until('native debug surface and running JACK device', () => window.__lf && settings() && engineDevice(), 18000);
  settings().click();
  const choice = await configure();
  if (config.phase === 'choose') {
    report('chosen', choice);
  } else {
    report('reopened', choice);
    const measured = await take(choice.device);
    report('take', measured);
  }
  // Let the normal WebView shutdown flush the settings to the private profile between launches.
  await confirmNativeClose();
}

void run().catch((error) => {
  window.__lf?.master.setMuted(true);
  report('fail', { error: String(error) });
});
