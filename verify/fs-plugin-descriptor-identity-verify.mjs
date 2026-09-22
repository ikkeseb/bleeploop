// fs-plugin-descriptor-identity-verify.mjs — native plugin picker identity and scan reconciliation.
//
// Imports the REAL source (Node TS type-stripping), not a port, so it cannot drift.

import assert from 'node:assert';
import {
  pluginDescriptorKey,
  pluginPickerLabel,
  reconcilePluginDescriptors,
  samePluginDescriptor,
} from '../src/audio/plugin-descriptor.ts';

let passed = 0;
let failed = 0;

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

const rootCopy = {
  id: '01efcdab8291ebfa4e4453504e4a5058',
  name: 'Archetype Petrucci X',
  format: 'vst3',
  path: 'C:\\Program Files\\Common Files\\VST3\\Archetype Petrucci X.vst3',
  isEffect: true,
};
const vendorCopy = {
  ...rootCopy,
  path: 'C:\\Program Files\\Common Files\\VST3\\Neural DSP\\Archetype Petrucci X.vst3',
};

check('same class id at distinct paths has distinct picker identities', () => {
  assert.notStrictEqual(pluginDescriptorKey(rootCopy), pluginDescriptorKey(vendorCopy));
  assert.strictEqual(samePluginDescriptor(rootCopy, vendorCopy), false);
});

check('exact scan repeats collapse while distinct paths survive', () => {
  assert.deepStrictEqual(
    reconcilePluginDescriptors([rootCopy, rootCopy, vendorCopy], []),
    [rootCopy, vendorCopy],
  );
});

check('a loaded orphan is retained once by its complete identity', () => {
  const missing = { ...rootCopy, id: 'missing', path: 'D:\\Plugins\\Missing.vst3' };
  assert.deepStrictEqual(
    reconcilePluginDescriptors([rootCopy, vendorCopy], [vendorCopy, missing]),
    [rootCopy, vendorCopy, missing],
  );
});

check('colliding labels identify the installed location', () => {
  const peers = [rootCopy, vendorCopy];
  assert.strictEqual(pluginPickerLabel(rootCopy, peers), 'Archetype Petrucci X (vst3) · VST3');
  assert.strictEqual(pluginPickerLabel(vendorCopy, peers), 'Archetype Petrucci X (vst3) · Neural DSP');
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
