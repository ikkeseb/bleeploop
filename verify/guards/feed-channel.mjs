// verify/guards/feed-channel.mjs — retired feed Channels release their registered callbacks,
// even when a missing message index prevents Tauri's ordinary end-of-channel cleanup.
// Runs the real platform host and its installed Channel under the module hooks, with scripted
// native replies and a fake clock. Covers watchdog replacement, unsubscribe and subscribe failure;
// cannot see WebView2, garbage collection or Tauri's native map of unfetched large payloads.
// Run: node verify/guards/feed-channel.mjs

import assert from 'node:assert/strict';
import { mock } from 'node:test';
import { setImmediate as settle } from 'node:timers/promises';
import '../harness/hooks.ts';

let passed = 0;
let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
  } catch (err) {
    failed++;
    console.error(`FAIL: ${name}\n  ${err.message}`);
  }
}

const callbacks = new Map();
const subscriptions = [];
const acks = [];
let nextId = 1;
let stuck = false;
let feedReply = () => Promise.resolve();
const internals = {
  transformCallback(callback) {
    const id = nextId++;
    callbacks.set(id, callback);
    return id;
  },
  unregisterCallback(id) {
    callbacks.delete(id);
  },
  invoke(command, args) {
    if (command === 'engine_feed') {
      // Keep only the public id: the harness must not hold the Channel alive itself.
      subscriptions.push(args.channel.id);
      return feedReply();
    }
    if (command === 'engine_feed_ack') {
      acks.push(args.seq);
      return Promise.resolve(stuck);
    }
    throw new Error(`Unexpected IPC command: ${command}`);
  },
};
globalThis.window = { __TAURI_INTERNALS__: internals };
mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
const { tauriPlatform } = await import('../../src/platform/host.tauri.ts');
const frame = (seq) => ({ seq, reset: false, events: [], device: [], status: null, anchor: null, meter: null, peaks: [] });
const received = [];
let stop = tauriPlatform.engine.subscribe((value) => received.push(value.seq));
await settle();
const first = subscriptions.at(-1);
// Retain this callback solely to simulate a delivery already queued before unregisterCallback.
const queuedFirst = callbacks.get(first);
queuedFirst({ index: 1, message: frame(11) });
queuedFirst({ index: 2, message: frame(12) });
queuedFirst({ index: 3, end: true });
await check('an index gap parks frames and prevents ordinary end cleanup', () => {
  assert.deepEqual(received, []);
  assert.ok(callbacks.has(first));
});

stuck = true;
mock.timers.tick(1000);
await settle();
stuck = false;
const second = subscriptions.at(-1);
await check('the heartbeat resubscribes after a stuck ack', () => {
  assert.deepEqual(acks, [null]);
  assert.equal(subscriptions.length, 2);
  assert.notEqual(second, first);
  assert.ok(callbacks.has(second), 'replacement callback must remain registered');
});
await check('watchdog replacement unregisters the gapped Channel', () => {
  assert.equal(callbacks.has(first), false, 'retired Channel callback is still registered');
});
await check('a queued retired delivery is ignored; the replacement delivers and acks', async () => {
  queuedFirst({ index: 0, message: frame(10) });
  assert.deepEqual(received, []);
  callbacks.get(second)({ index: 0, message: frame(20) });
  assert.deepEqual(received, [20]);
  mock.timers.tick(100);
  await settle();
  assert.deepEqual(acks, [null, 20]);
});
stop();
await check('unsubscribe retires the live Channel and stops ack timers', async () => {
  assert.equal(callbacks.has(second), false, 'unsubscribed Channel callback is still registered');
  mock.timers.tick(1000);
  await settle();
  assert.deepEqual(acks, [null, 20]);
});

const subscribeError = new Error('scripted subscribe failure');
feedReply = () => Promise.reject(subscribeError);
const errors = [];
const realError = console.error;
try {
  console.error = (...args) => errors.push(args);
  stop = tauriPlatform.engine.subscribe(() => {});
  await settle();
} finally {
  console.error = realError;
}
const rejected = subscriptions.at(-1);
await check('a failed subscribe retires its own Channel', () => {
  assert.equal(callbacks.has(rejected), false, 'failed subscribe Channel callback is still registered');
});
await check('a failed subscribe still reports its error', () => {
  assert.deepEqual(errors, [['[host.tauri] engine feed subscribe failed', subscribeError]]);
});
stop();

// A late rejection from a superseded subscription must not retire its replacement.
let rejectOld;
feedReply = () => new Promise((_, reject) => { rejectOld = reject; });
const stopOld = tauriPlatform.engine.subscribe(() => {});
const old = subscriptions.at(-1);
feedReply = () => Promise.resolve();
stop = tauriPlatform.engine.subscribe(() => {});
const current = subscriptions.at(-1);
rejectOld(subscribeError);
await settle();
await check('replacing a subscriber retires it even before its invoke settles', () => {
  assert.equal(callbacks.has(old), false, 'superseded subscriber callback is still registered');
  assert.ok(callbacks.has(current));
  stopOld();
  assert.ok(callbacks.has(current), 'old unsubscribe must not retire the current subscriber');
});
await check('missing Tauri internals make retirement a no-op', () => {
  delete window.__TAURI_INTERNALS__;
  try {
    assert.doesNotThrow(stop);
  } finally {
    window.__TAURI_INTERNALS__ = internals;
    internals.unregisterCallback(current);
  }
});

mock.timers.reset();
console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
