// Real synth/router ownership probe. Requires the browser verification rig; no MIDI hardware.
// Each assertion follows rendered audio or Tone's actual voice-allocation rejection.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  const warnings = [];
  page.on('console', (message) => { if (message.text().includes('Note dropped')) warnings.push(message.text()); });
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);
  const result = await page.evaluate(async () => {
    const { engine, inputRouter: router } = window.__lf;
    await engine.start();
    const { SYNTHS } = await import('/src/audio/synths/index.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const analyser = engine.ctx.createAnalyser(); analyser.fftSize = 4096;
    engine.instrumentBus.connect(analyser);
    const samples = new Float32Array(analyser.fftSize);
    const rms = () => {
      analyser.getFloatTimeDomainData(samples);
      return Math.sqrt(samples.reduce((sum, value) => sum + value * value, 0) / samples.length);
    };
    const amplitude = (note) => {
      analyser.getFloatTimeDomainData(samples);
      const frequency = 440 * 2 ** ((note - 69) / 12);
      let real = 0, imaginary = 0;
      for (let k = 0; k < samples.length; k++) {
        const angle = 2 * Math.PI * frequency * k / engine.ctx.sampleRate;
        real += samples[k] * Math.cos(angle); imaginary += samples[k] * Math.sin(angle);
      }
      return 2 * Math.hypot(real, imaginary) / samples.length;
    };
    const on = (note) => router.handle({ type: 'on', note, velocity: 110, source: 'computer' });
    const off = (note) => router.handle({ type: 'off', note, velocity: 0, source: 'computer' });
    router.setActivePlugin(null); router.setSustain(false); router.allNotesOff();
    const bassResults = [];
    for (const released of [48, 55]) {
      const voice = SYNTHS.find((factory) => factory.id === 'bass').create();
      router.setActiveEngine(voice);
      on(48); await pause(180); on(55); await pause(180);
      const before = rms(); off(released); await pause(650);
      const expected = released === 48 ? 55 : 48;
      const row = { released, stillHeld: [...router.held], before, after: rms(), expectedPitch: amplitude(expected), wrongPitch: amplitude(released) };
      off(expected); await pause(650); row.fullyReleased = rms();
      bassResults.push(row);
      router.setActiveEngine(null); voice.dispose(); await pause(150);
    }
    const lead = SYNTHS.find((factory) => factory.id === 'lead').create();
    router.setActiveEngine(lead); router.setSustain(true);
    for (let note = 48; note < 64; note++) {
      on(note); await pause(20); off(note); await pause(20);
    }
    await pause(120);
    const sustainRms = rms();
    router.setSustain(false); router.setActiveEngine(null); lead.dispose();
    await pause(150);
    const stealing = [];
    for (const id of ['lead', 'pad', 'piano', 'organ']) {
      const voice = SYNTHS.find((factory) => factory.id === id).create();
      router.setActiveEngine(voice);
      const count = id === 'lead' || id === 'organ' ? 8 : 12;
      for (let note = 36; note < 36 + count; note++) on(note);
      on(83); // fresh high note when every bounded voice is occupied
      await pause(id === 'pad' ? 1000 : 200);
      const fresh = amplitude(83);
      off(36); // old note-off for the slot reused by the high note
      await pause(180);
      const afterStaleRelease = amplitude(83);
      router.allNotesOff(); await pause(id === 'pad' ? 2300 : 1100);
      stealing.push({ id, fresh, afterStaleRelease, fullyReleased: rms() });
      router.setActiveEngine(null); voice.dispose();
    }
    engine.instrumentBus.disconnect(analyser);
    return { bassResults, sustainRms, stealing };
  });
  console.log(JSON.stringify({ ...result, droppedAttacks: warnings.length, warnings }));
  for (const row of result.bassResults) {
    assert.ok(row.before > 0.01, 'bass must produce signal before releasing either overlapping note');
    assert.ok(row.after > row.before * 0.2, 'releasing one bass note must preserve the other held note');
    assert.ok(row.expectedPitch > row.wrongPitch * 3, 'bass must sound the newest remaining held pitch');
    assert.ok(row.fullyReleased < 1e-5, 'releasing the final bass key must finish its envelope');
  }
  for (const row of result.stealing) {
    assert.ok(row.fresh > 0.002, `${row.id}: new attack must sound with a full voice pool`);
    assert.ok(row.afterStaleRelease > row.fresh * 0.15, `${row.id}: stolen note release must not kill its replacement`);
    assert.ok(row.fullyReleased < 1e-5, `${row.id}: panic must release every voice`);
  }
  assert.equal(warnings.length, 0, 'sustain voice management must preserve fresh attacks during a fast run');
} finally { await browser.close(); }
