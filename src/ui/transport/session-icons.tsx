import type { JSX } from 'solid-js';

/**
 * The session tools' icons, drawn once: the command bar's EXPORT and IMPORT tools (`SessionTools.tsx`)
 * and Help's Manual, which shows the reader the icon to look for. Sized by the caller's CSS.
 */
export const ExportIcon = (): JSX.Element => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
    <path d="M12 4v11M7.5 10.5 12 15l4.5-4.5" stroke-linecap="round" stroke-linejoin="round" />
    <path d="M4 17v2.5h16V17" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
);

export const ImportIcon = (): JSX.Element => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
    <path d="M12 15V4M7.5 8.5 12 4l4.5 4.5" stroke-linecap="round" stroke-linejoin="round" />
    <path d="M4 17v2.5h16V17" stroke-linecap="round" stroke-linejoin="round" />
  </svg>
);
