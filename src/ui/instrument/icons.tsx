/**
 * The slot row's line icons, drawn on one 18-unit grid with one stroke (the mockups' set,
 * `docs/redesign/controls.js`): currentColor, so a glyph takes its control's legend tone.
 */
const PATHS = {
  drop: 'M5 7l4 4 4-4',
  win: 'M2.5 5h9.5v9.5H2.5zM9 2.5h6.5V9M15.5 2.5L9.5 8.5',
  keys: 'M2.5 5h13v8h-13zM6 5v5M9 5v5M12 5v5',
  params: 'M3 5h12M3 9h12M3 13h12',
  swap: 'M3.5 6.5h11M11.5 3.5l3 3-3 3M14.5 11.5h-11M6.5 8.5l-3 3 3 3',
  x: 'M4.5 4.5l9 9M13.5 4.5l-9 9',
  left: 'M10.5 4.5L6 9l4.5 4.5',
  right: 'M7.5 4.5L12 9l-4.5 4.5',
  plus: 'M9 3.5v11M3.5 9h11',
} as const;

/** A glyph's name in the row's set. */
export type IconName = keyof typeof PATHS;

export function Icon(props: { name: IconName; size?: number }) {
  const s = () => props.size ?? 16;
  return (
    <svg
      class="slot-icon"
      width={s()}
      height={s()}
      viewBox="0 0 18 18"
      fill="none"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d={PATHS[props.name]} />
    </svg>
  );
}
