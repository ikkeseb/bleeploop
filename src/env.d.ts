/// <reference types="vite/client" />

/** package.json's version, set at build time (`vite.config.ts` `define`). */
declare const __APP_VERSION__: string;
/** The short commit the bundle was built from, or "unknown" (`vite.config.ts` `define`). */
declare const __APP_COMMIT__: string;
