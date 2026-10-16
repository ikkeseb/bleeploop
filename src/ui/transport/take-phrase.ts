/**
 * OWNS: the TAKE chip's phrase, the record modes read as one line (`TakeModes.tsx`).
 *
 * The grammar: the take's length leads and is always there, `FIXED n` with FIXED on (n is the bars the
 * next take records) or `FREE` with it off, so the phrase is never empty. The modes that are on follow
 * in the popover's order, RETAKE, AUTO REC, CLICK, END STOP, joined by ` · `. The phrase never runs past
 * `TAKE_PHRASE_MAX` characters: the first mode that would push it (or the ` +k` it would then need) past
 * that ends it, and ` +k` counts the k modes left unsaid ("FIXED 4 · RETAKE +3"). The chip reserves
 * the width of the widest phrase this rule can produce, so nothing beside it moves when a mode turns.
 * `verify/guards/take-phrase.mjs` holds the rule over every combination.
 */

export const TAKE_PHRASE_MAX = 22;

/** One term of the phrase; `key` says which mode it reads, so the chip can dim one that does not apply. */
export interface TakeTerm {
  key: 'fixed' | 'free' | 'retake' | 'autoRec' | 'click' | 'endStop';
  text: string;
}

export interface TakePhrase {
  terms: TakeTerm[];
  /** The modes that are on but left unsaid. */
  more: number;
}

export interface TakeModeState {
  /** The next take's bars with FIXED on; null with it off. */
  fixedBars: number | null;
  retake: boolean;
  autoRec: boolean;
  click: boolean;
  endStop: boolean;
}

const SEP = ' · ';
const moreText = (k: number): string => (k > 0 ? ` +${k}` : '');

export function takePhrase(m: TakeModeState): TakePhrase {
  const lead: TakeTerm = m.fixedBars === null ? { key: 'free', text: 'FREE' } : { key: 'fixed', text: `FIXED ${m.fixedBars}` };
  const on: TakeTerm[] = [];
  if (m.retake) on.push({ key: 'retake', text: 'RETAKE' });
  if (m.autoRec) on.push({ key: 'autoRec', text: 'AUTO REC' });
  if (m.click) on.push({ key: 'click', text: 'CLICK' });
  if (m.endStop) on.push({ key: 'endStop', text: 'END STOP' });
  const terms = [lead];
  let length = lead.text.length;
  for (let i = 0; i < on.length; i++) {
    const next = length + SEP.length + on[i].text.length;
    if (next + moreText(on.length - i - 1).length > TAKE_PHRASE_MAX) return { terms, more: on.length - i };
    terms.push(on[i]);
    length = next;
  }
  return { terms, more: 0 };
}

/** The phrase as the chip shows it, as one string. @public `verify/guards/take-phrase.mjs` reads the rule
 * through it (the chip renders the terms one by one). */
export function takePhraseText(p: TakePhrase): string {
  return p.terms.map((t) => t.text).join(SEP) + moreText(p.more);
}
