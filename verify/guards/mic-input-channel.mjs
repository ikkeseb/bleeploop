// verify/guards/mic-input-channel.mjs — deterministic guard for the web MIC input channel selector.
//
// Imports the REAL platform implementation and supplies a tiny fake Web Audio graph. The regression
// caught here is the rig finding from 2026-08-29: Audio Settings persisted Ch 1 / Ch 2, but MIC LIVE
// opened the same generic getUserMedia stream and then summed every returned channel. An explicit
// channel pick must request enough channels, route only that ChannelSplitter output, and keep the
// returned node mono so capture.ts can centre it through its existing mono node.

import assert from 'node:assert';

function fakeNode(kind) {
  return {
    kind,
    connections: [],
    disconnects: 0,
    channelCount: 2,
    channelCountMode: 'max',
    channelInterpretation: 'speakers',
    connect(target, output = 0, input = 0) {
      this.connections.push({ target, output, input });
      return target;
    },
    disconnect() {
      this.disconnects++;
    },
  };
}

function makeFixture() {
  const listeners = new Map();
  const track = {
    stopped: 0,
    addEventListener(type, fn) {
      listeners.set(type, fn);
    },
    removeEventListener(type, fn) {
      if (listeners.get(type) === fn) listeners.delete(type);
    },
    stop() {
      this.stopped++;
    },
    emit(type) {
      listeners.get(type)?.();
    },
  };
  const stream = { getTracks: () => [track] };
  const source = fakeNode('source');
  const created = { splitters: [], gains: [] };
  const ctx = {
    sampleRate: 48_000,
    createMediaStreamSource(received) {
      assert.strictEqual(received, stream);
      return source;
    },
    createChannelSplitter(outputs) {
      const node = fakeNode('splitter');
      node.outputs = outputs;
      created.splitters.push(node);
      return node;
    },
    createGain() {
      const node = fakeNode('gain');
      created.gains.push(node);
      return node;
    },
  };
  return { ctx, stream, track, source, created };
}

let constraints;
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: {
    mediaDevices: {
      async getUserMedia(received) {
        constraints = received;
        return currentFixture.stream;
      },
    },
  },
});

const { webPlatform } = await import('../../src/platform/host.web.ts');

let passed = 0;
let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

let currentFixture;

await check('Ch 2 requests two channels and routes only splitter output 1', async () => {
  currentFixture = makeFixture();
  const opened = await webPlatform.audioInput.open(currentFixture.ctx, { channel: 1 });
  const splitter = currentFixture.created.splitters[0];
  const selected = currentFixture.created.gains[0];

  assert.deepStrictEqual(constraints.audio.channelCount, { min: 2 });
  assert.strictEqual(currentFixture.created.splitters.length, 1);
  assert.strictEqual(splitter.outputs, 2);
  assert.deepStrictEqual(currentFixture.source.connections, [{ target: splitter, output: 0, input: 0 }]);
  assert.deepStrictEqual(splitter.connections, [{ target: selected, output: 1, input: 0 }]);
  assert.strictEqual(opened.node, selected);
  assert.strictEqual(selected.channelCount, 1);
  assert.strictEqual(selected.channelCountMode, 'explicit');
  assert.strictEqual(selected.channelInterpretation, 'discrete');

  opened.close();
  assert.strictEqual(currentFixture.track.stopped, 1);
  assert.strictEqual(currentFixture.source.disconnects, 1);
  assert.strictEqual(splitter.disconnects, 1);
  assert.strictEqual(selected.disconnects, 1);
});

await check('auto keeps the existing advisory mono request and direct source node', async () => {
  currentFixture = makeFixture();
  const opened = await webPlatform.audioInput.open(currentFixture.ctx);

  assert.strictEqual(constraints.audio.channelCount, 1);
  assert.strictEqual(opened.node, currentFixture.source);
  assert.strictEqual(currentFixture.created.splitters.length, 0);
  assert.strictEqual(currentFixture.created.gains.length, 0);
  opened.close();
});

await check('selected-channel input loss still reports once and never on close', async () => {
  currentFixture = makeFixture();
  let losses = 0;
  const opened = await webPlatform.audioInput.open(currentFixture.ctx, {
    channel: 0,
    onLost: () => losses++,
  });

  assert.deepStrictEqual(constraints.audio.channelCount, { min: 2 });
  currentFixture.track.emit('ended');
  currentFixture.track.emit('ended');
  assert.strictEqual(losses, 1);
  opened.close();
  currentFixture.track.emit('ended');
  assert.strictEqual(losses, 1);
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
