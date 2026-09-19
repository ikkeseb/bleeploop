// Executes the real capture processor in Node with a worklet-global shim and real ringbuf.js SABs.
// Browser scheduling is covered separately by capture-clock.mjs.
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';
import { RingBuffer } from 'ringbuf.js';
import { CAPTURE_PACKET_SIZE, CAPTURE_PACKET_HEADER, capturePacketCapacity } from '../src/audio/capture-packet.ts';

let Processor;
globalThis.AudioWorkletProcessor = class {};
globalThis.registerProcessor = (name, ctor) => {
  assert.equal(name, 'capture-processor');
  Processor = ctor;
};
globalThis.currentFrame = 0;
// Vite resolves this extensionless TS import in production. Node needs the explicit suffix.
const hooks = registerHooks({ resolve(specifier, context, nextResolve) {
  return nextResolve(specifier === '../capture-packet' ? `${specifier}.ts` : specifier, context);
} });
try { await import('../src/audio/worklets/capture-processor.ts'); }
finally { hooks.deregister(); }

let passed = 0;
let failed = 0;
function check(name, fn) {
  try { fn(); passed++; }
  catch (error) { failed++; console.error(`FAIL: ${name}\n${error.stack}`); }
}
function fixture(packets = 2) {
  const ringSab = RingBuffer.getStorageForCapacity(packets * CAPTURE_PACKET_SIZE, Float64Array);
  const heartbeatSab = new SharedArrayBuffer(8);
  return { processor: new Processor({ processorOptions: { ringSab, heartbeatSab } }),
    ring: new RingBuffer(ringSab, Float64Array), heartbeat: new Int32Array(heartbeatSab) };
}
const input = Float32Array.from({ length: 128 }, (_, k) => (k % 2 ? -1 : 1) * (k + 1) / 997);
const packet = new Float64Array(CAPTURE_PACKET_SIZE);
const rig = fixture();
const origin = 2 ** 40 + 128;
for (let n = 0; n < 3; n++) {
  globalThis.currentFrame = origin + n * 128;
  rig.processor.process([[input]]);
}
check('full ring drops exactly one complete quantum', () => {
  assert.equal(rig.ring.available_read(), 2 * CAPTURE_PACKET_SIZE);
  assert.equal(rig.heartbeat[0], 3);
  assert.equal(rig.heartbeat[1], 128);
});
check('timestamp retains long-session frame precision and PCM stays Float32-exact', () => {
  assert.equal(rig.ring.pop(packet), CAPTURE_PACKET_SIZE);
  assert.equal(packet[0], origin);
  assert.equal(packet[1], 128);
  assert.deepEqual(Float32Array.from(packet.subarray(CAPTURE_PACKET_HEADER)), input);
});
globalThis.currentFrame = origin + 384;
rig.processor.process([[]]);
check('old packet survives producer wrap without timestamp/PCM reassociation', () => {
  assert.equal(rig.ring.pop(packet), CAPTURE_PACKET_SIZE);
  assert.equal(packet[0], origin + 128);
  assert.deepEqual(Float32Array.from(packet.subarray(CAPTURE_PACKET_HEADER)), input);
});
check('after a drop the next timestamp exposes the gap and disconnected input is silent', () => {
  assert.equal(rig.ring.pop(packet), CAPTURE_PACKET_SIZE);
  assert.equal(packet[0], origin + 384);
  assert.ok(packet.subarray(CAPTURE_PACKET_HEADER).every((sample) => sample === 0));
  assert.equal(rig.ring.available_read(), 0);
  assert.equal(rig.heartbeat[1], 128);
});
for (let n = 0; n < 8; n++) {
  check(`ring wrap ${n} preserves absolute frame and samples`, () => {
    globalThis.currentFrame = origin + 512 + n * 128;
    rig.processor.process([[input]]);
    assert.equal(rig.ring.pop(packet), CAPTURE_PACKET_SIZE);
    assert.equal(packet[0], globalThis.currentFrame);
    assert.deepEqual(Float32Array.from(packet.subarray(CAPTURE_PACKET_HEADER)), input);
  });
}
check('packet capacity preserves at least the original audio-frame budget', () => {
  for (const frames of [1, 128, 129, 524288]) {
    const capacity = capturePacketCapacity(frames);
    assert.equal(capacity % CAPTURE_PACKET_SIZE, 0);
    assert.ok(capacity / CAPTURE_PACKET_SIZE * 128 >= frames);
  }
});
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exitCode = failed ? 1 : 0;
