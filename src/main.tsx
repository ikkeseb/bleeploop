import { render } from 'solid-js/web';
import { App } from './app.tsx'; // explicit: a sibling src/app/ directory exists
import './app.css';

const root = document.getElementById('root');
if (!root) throw new Error('#root element not found');

render(() => <App />, root);
