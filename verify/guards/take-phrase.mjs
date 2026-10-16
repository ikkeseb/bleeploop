#!/usr/bin/env node
// verify/guards/take-phrase.mjs — deterministic guard for the TAKE chip's phrase rule in
// src/ui/transport/take-phrase.ts, over every mode combination at 1 to 150 bars: never empty, never past
// TAKE_PHRASE_MAX, the length first, the modes that are on in the popover's order, and every mode
// either said or counted in ` +k`. The chip's pixel width (its sizer) is a rendered fact: the
// contact-sheet scenes show it, nothing here measures it.
// Run: node verify/guards/take-phrase.mjs

import { TAKE_PHRASE_MAX, takePhrase, takePhraseText } from '../../src/ui/transport/take-phrase.ts';

let passed = 0;
let failed = 0;

function check(name, condition, detail = '') {
  if (condition) {
    passed++;
    console.log(`  ok   ${name}${detail ? `  ${detail}` : ''}`);
  } else {
    failed++;
    console.log(`  FAIL ${name}${detail ? `  ${detail}` : ''}`);
  }
}

const ORDER = ['RETAKE', 'AUTO REC', 'CLICK', 'END STOP'];

console.log('=== A. the examples read as designed ===');
{
  const say = (m) => takePhraseText(takePhrase({ fixedBars: null, retake: false, autoRec: false, click: false, endStop: false, ...m }));
  check('A nothing on reads FREE', say({}) === 'FREE', say({}));
  check('A FIXED 4 with CLICK', say({ fixedBars: 4, click: true }) === 'FIXED 4 · CLICK', say({ fixedBars: 4, click: true }));
  check('A FREE with AUTO REC', say({ autoRec: true }) === 'FREE · AUTO REC', say({ autoRec: true }));
  const all = say({ fixedBars: 4, retake: true, autoRec: true, click: true, endStop: true });
  check('A every mode on counts the unsaid ones', all === 'FIXED 4 · RETAKE +3', all);
}

console.log('=== B. every combination, 1 to 150 bars ===');
{
  let longest = '';
  const bad = [];
  for (let bars = 0; bars <= 150; bars++) {
    for (let m = 0; m < 16; m++) {
      const flags = [!!(m & 1), !!(m & 2), !!(m & 4), !!(m & 8)];
      const [retake, autoRec, click, endStop] = flags;
      const p = takePhrase({ fixedBars: bars === 0 ? null : bars, retake, autoRec, click, endStop });
      const text = takePhraseText(p);
      const lead = bars === 0 ? 'FREE' : `FIXED ${bars}`;
      const on = ORDER.filter((_, i) => flags[i]);
      const said = p.terms.slice(1).map((t) => t.text);
      const ok =
        text.length > 0 &&
        text.length <= TAKE_PHRASE_MAX &&
        p.terms[0]?.text === lead &&
        said.every((t, i) => t === on[i]) &&
        said.length + p.more === on.length &&
        // a mode is left unsaid only when saying it would not fit
        (p.more === 0 || `${[lead, ...said, on[said.length]].join(' · ')}${p.more > 1 ? ` +${p.more - 1}` : ''}`.length > TAKE_PHRASE_MAX);
      if (!ok) bad.push(text);
      if (text.length > longest.length) longest = text;
    }
  }
  check('B every phrase holds the rule', bad.length === 0, bad.slice(0, 3).join(' | '));
  check(`B the longest stays within ${TAKE_PHRASE_MAX} characters`, longest.length <= TAKE_PHRASE_MAX, `"${longest}" (${longest.length})`);
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
