/** A built-in instrument as the slot picker lists it; the engine plays it (`InstrumentId`). */
export interface BuiltinInstrument {
  readonly id: string;
  readonly name: string;
}

/** The six built-in instruments. Array order = the UI's voice order. */
export const SYNTHS: readonly BuiltinInstrument[] = [
  { id: 'lead', name: 'Lead' },
  { id: 'bass', name: 'Bass' },
  { id: 'pad', name: 'Pad' },
  { id: 'piano', name: 'Piano' },
  { id: 'organ', name: 'Organ' },
  { id: 'drum', name: 'Drum' },
];
