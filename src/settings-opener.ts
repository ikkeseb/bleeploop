/**
 * OWNS: the one way to open Audio Settings on a page from outside the panel: a toast's action, Help's
 * link, an empty slot's line. The panel's open and page signals stay in `src/app.tsx`, which registers
 * the opener (it also closes Help and Update: one popover at a time); `__lf.ui.openSettings` calls the
 * same one.
 *
 * BOUNDARY: a leaf at `src/` root beside `src/notify.ts`, for the same reason: every layer, `src/platform/`
 * too (its "No audio device" toast), may import it. It imports nothing.
 */

/** The panel's pages, in the rail's order (`src/ui/settings/AudioSettings.tsx`). */
export type SettingsPage = 'device' | 'midi' | 'plugins' | 'diagnostics';

/** Each page's name as the rail shows it. */
export const SETTINGS_PAGE_LABELS: Readonly<Record<SettingsPage, string>> = {
  device: 'Device',
  midi: 'MIDI pedals',
  plugins: 'Plugins',
  diagnostics: 'Diagnostics',
};

let opener: ((page: SettingsPage) => void) | null = null;

/** app.tsx: register the opener. Returns the unregister. */
export function registerSettingsOpener(open: (page: SettingsPage) => void): () => void {
  opener = open;
  return () => {
    if (opener === open) opener = null;
  };
}

/** Open Audio Settings on `page` (switch to it when the panel is open already). */
export function openSettings(page: SettingsPage): void {
  opener?.(page);
}

/** The way to `page` as every toast, title and Help line writes it: the rail's name for the page. */
export function settingsPath(page: SettingsPage): string {
  return `Audio Settings → ${SETTINGS_PAGE_LABELS[page].toUpperCase()}`;
}

/** A toast's action that opens Audio Settings on `page`. */
export function settingsAction(page: SettingsPage): { label: string; run: () => void } {
  return { label: settingsPath(page), run: () => openSettings(page) };
}
