import { render } from 'solid-js/web';
import './app.css';
import { App } from './app.tsx';

const root = document.getElementById('root');
if (!root) throw new Error('#root element not found');

render(() => <App />, root);
