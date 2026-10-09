/**
 * A drum voice's static descriptor — the single source of truth for the kit's note set, display
 * label, and PC-keyboard pad key. Consumed by the on-screen drum-pad UI (src/ui/keyboard/Keyboard.tsx),
 * the PC-keyboard map (src/app/transport-keys.ts) and Help. A MIDI controller plays the same `note`
 * numbers straight through native MIDI's note router; the `key` is PC-keyboard-only.
 */
export interface DrumVoice {
  readonly note: number; // GM percussion MIDI note
  readonly label: string; // short display name for the pad
  readonly key: string; // PC-keyboard key that fires this pad
}

/**
 * The kit, in pad-grid order (4 columns × 4 rows). The UI renders these left-to-right then
 * top-to-bottom, so the array order IS the visual layout. Key rows: 1234 / qwer / asdf / zxcv.
 * The home row (a/s/d/f) holds the four essentials so they sit under the resting hand.
 *
 * GM-inspired note map (notes are standard General MIDI percussion assignments):
 *   49 Crash · 51 Ride · 56 Cowbell · 54 Tambourine
 *   39 Clap · 40 Electric snare · 44 Pedal hat · 37 Rim/side-stick
 *   36 Kick · 38 Snare · 42 Closed hat · 46 Open hat
 *   35 Kick 2 (acoustic BD) · 45 Low tom · 47 Mid tom · 50 High tom
 */
export const DRUM_KIT: ReadonlyArray<DrumVoice> = [
  // row 1 — cymbals & accents
  { note: 49, label: 'Crash', key: '1' },
  { note: 51, label: 'Ride', key: '2' },
  { note: 56, label: 'Cowbell', key: '3' },
  { note: 54, label: 'Tamb', key: '4' },
  // row 2 — extra snares & hats
  { note: 39, label: 'Clap', key: 'q' },
  { note: 40, label: 'E-Snr', key: 'w' },
  { note: 44, label: 'P-Hat', key: 'e' },
  { note: 37, label: 'Rim', key: 'r' },
  // row 3 — the essentials
  { note: 36, label: 'Kick', key: 'a' },
  { note: 38, label: 'Snare', key: 's' },
  { note: 42, label: 'CH', key: 'd' },
  { note: 46, label: 'OH', key: 'f' },
  // row 4 — second kick & toms
  { note: 35, label: 'Kick 2', key: 'z' },
  { note: 45, label: 'Lo Tom', key: 'x' },
  { note: 47, label: 'Mid Tom', key: 'c' },
  { note: 50, label: 'Hi Tom', key: 'v' },
];
