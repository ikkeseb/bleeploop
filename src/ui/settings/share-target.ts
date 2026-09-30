/**
 * Does a Share output device look like the interface the ASIO driver plays on? Tester report F22: with
 * Share on the interface in the player's ears, the master is heard twice, the mirror slightly late. The
 * ASIO driver and the Windows endpoint name the same hardware differently ("Focusrite USB ASIO" vs
 * "Speakers (2- Focusrite USB Audio)"), so this compares the brand/model words both carry, after
 * dropping the generic ones. A wrapper driver such as ASIO4ALL names no hardware, so it never matches:
 * the note stays quiet rather than guessing. Pure, no DOM: `verify/guards/share-target.mjs`.
 */

/** Words that describe a kind of device or connection, not which one: they appear on both sides of
 * unrelated pairs ("Realtek ASIO" / "CABLE Input (VB-Audio...)" both say "audio"). */
const GENERIC = new Set([
  'asio', 'usb', 'audio', 'driver', 'device', 'speakers', 'speaker', 'headphones', 'line', 'output',
  'input', 'out', 'digital', 'interface', 'stereo', 'sound', 'high', 'definition', 'wdm', 'the', 'for',
  'pro',
]);

/** The name's distinguishing tokens: lowercase words of 3+ letters, not pure numbers, not generic. */
function tokens(name: string): string[] {
  return name
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((t) => t.length >= 3 && !/^\d+$/.test(t) && !GENERIC.has(t));
}

/** True when any distinguishing word of the ASIO driver's name appears in the Share device's name. */
export function sharesInterface(asioName: string, shareName: string): boolean {
  const share = new Set(tokens(shareName));
  return tokens(asioName).some((t) => share.has(t));
}
