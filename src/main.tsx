import { render } from 'solid-js/web';
import { App } from './app.tsx'; // explicit: a sibling src/app/ directory exists
import { resolveEngineMode } from './platform';
import './app.css';

const root = document.getElementById('root');
if (!root) throw new Error('#root element not found');
const mount = root;

// The engine toggle is read once, before anything renders: every audio path branches on it
// (`engineMode()`), and it never changes while the app runs.
void resolveEngineMode().then(() => render(() => <App />, mount));
