import { render } from 'solid-js/web';
import { resolveEngineMode } from './platform';
import './app.css';

const root = document.getElementById('root');
if (!root) throw new Error('#root element not found');
const mount = root;

/**
 * Hide the AudioContext constructors while the app's modules load; returns the undo. Tone builds its
 * default AudioContext the moment its module loads (`var Transport = getContext().transport`), and
 * WebView2 starts it running: an output stream on the Windows default device beside the engine's. With
 * no constructor in sight Tone keeps its inert dummy context. Engine mode plays nothing through Web
 * Audio, so nothing asks for one later.
 */
function hideAudioContext(): () => void {
  const global = window as unknown as Record<string, unknown>;
  const saved = ['AudioContext', 'webkitAudioContext'].map((name) => [name, Object.getOwnPropertyDescriptor(window, name)] as const);
  for (const [name, descriptor] of saved) if (descriptor) delete global[name];
  return () => {
    for (const [name, descriptor] of saved) if (descriptor) Object.defineProperty(window, name, descriptor);
  };
}

// The engine toggle is read once, before the app loads: every audio path branches on it (`engineMode()`),
// and it never changes while the app runs. Engine mode loads the app with Web Audio hidden.
void resolveEngineMode().then(async (engine) => {
  const restore = engine ? hideAudioContext() : null;
  const { App } = await import('./app.tsx').finally(() => restore?.());
  render(() => <App />, mount);
});
