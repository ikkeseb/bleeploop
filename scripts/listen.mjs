// Render the ignored listening tests, or rebuild their self-contained phone sheet with --from.
import { execFile, spawn } from 'node:child_process';
import { mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = join(root, 'logs', 'listen');
const escapeHtml = (value) => String(value).replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
})[c]);
const clock = (seconds) => `${Math.floor(seconds / 60)}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;

async function render(clipsDir) {
  await rm(clipsDir, { recursive: true, force: true });
  await mkdir(clipsDir, { recursive: true });
  await new Promise((done, reject) => {
    const child = spawn('cargo', ['test', '-p', 'lf-engine', '--no-default-features', '--test', 'listen', '--', '--ignored'], {
      cwd: join(root, 'src-tauri'),
      env: { ...process.env, LF_LISTEN_OUT: clipsDir },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let tail = '';
    for (const [stream, destination] of [[child.stdout, process.stdout], [child.stderr, process.stderr]]) {
      stream.setEncoding('utf8');
      stream.on('data', (chunk) => {
        tail = (tail + chunk).slice(-6000);
        destination.write(chunk);
      });
    }
    child.on('error', (error) => reject(new Error(error.code === 'ENOENT'
      ? 'Cargo was not found. Install the Rust toolchain, or use --from with existing clips.'
      : `Cargo could not start: ${error.message}`)));
    child.on('close', (code, signal) => {
      if (code === 0) done();
      else reject(new Error(`Cargo failed (${signal || `exit ${code}`}):\n${tail.trim()}`));
    });
  });
}

async function readClips(folder) {
  const names = (await readdir(folder)).filter((name) => name.endsWith('.json')).sort();
  if (!names.length) throw new Error(`No clip JSON files found in ${folder}`);
  const clips = [];
  const audio = new Map();
  for (const name of names) {
    const clip = JSON.parse(await readFile(join(folder, name), 'utf8'));
    const fail = (why) => { throw new Error(`${name}: ${why}`); };
    for (const field of ['id', 'group', 'title', 'question', 'cannot_tell', 'file']) {
      if (typeof clip[field] !== 'string' || !clip[field]) fail(`missing ${field}`);
    }
    if (audio.has(clip.id)) fail(`duplicate clip id ${clip.id}`);
    if (!Number.isFinite(clip.order) || !Number.isFinite(clip.seconds) || clip.seconds <= 0) fail('invalid order or duration');
    if (basename(clip.file) !== clip.file || /[/\\]/.test(clip.file) || !clip.file.endsWith('.wav')) fail('file must be a WAV filename');
    if (!Array.isArray(clip.marks)) fail('marks must be an array');
    for (const mark of clip.marks) {
      if (typeof mark.label !== 'string' || !Number.isFinite(mark.t) || mark.t < 0 || mark.t > clip.seconds) fail('invalid mark or mark outside clip');
    }
    if (clip.b_start !== null && (!Number.isFinite(clip.b_start) || clip.b_start <= 0 || clip.b_start >= clip.seconds)) fail('B start outside clip');
    let wav;
    try { wav = await readFile(join(folder, clip.file)); }
    catch (error) { fail(`cannot read WAV ${clip.file}: ${error.message}`); }
    if (!wav.length) fail(`WAV ${clip.file} is empty`);
    audio.set(clip.id, wav.toString('base64'));
    clips.push(clip);
  }
  clips.sort((a, b) => a.order - b.order || a.id.localeCompare(b.id));
  return { clips, audio };
}

function page(manifest, audio) {
  const { commit, date, clips } = manifest;
  let group;
  const cards = clips.map((clip, index) => {
    const heading = clip.group === group ? '' : `<h2>${escapeHtml(clip.group)}</h2>`;
    group = clip.group;
    return `${heading}
<article class="clip" data-index="${index}" aria-labelledby="title-${index}">
  <h3 id="title-${index}">${escapeHtml(clip.title)}</h3>
  <p class="question">${escapeHtml(clip.question)}</p>
  <audio preload="metadata" src="data:audio/wav;base64,${audio.get(clip.id)}"></audio>
  <div class="player">
    <button type="button" class="play" aria-label="Play ${escapeHtml(clip.title)}">Play</button>
    <div class="timeline">
      <label class="sr-only" for="seek-${index}">Position in ${escapeHtml(clip.title)}</label>
      <input id="seek-${index}" class="seek" type="range" min="0" max="${clip.seconds}" step="0.01" value="0">
      <div class="ticks" aria-hidden="true">${clip.marks.map((mark) => `<span style="left:${100 * mark.t / clip.seconds}%"></span>`).join('')}</div>
    </div>
    <span class="time">0:00 / ${clock(clip.seconds)}</span>
  </div>
  <div class="chips marks">${clip.marks.map((mark) => `<button type="button" data-seek="${Math.max(0, mark.t - 0.5)}">${clock(mark.t)} ${escapeHtml(mark.label)}</button>`).join('')}</div>
  ${clip.b_start === null ? '' : `<div class="ab"><button type="button" data-seek="0" aria-label="Play A">A</button><button type="button" data-seek="${clip.b_start}" aria-label="Play B">B</button></div>`}
  <p class="play-error dim" role="status"></p>
  <p class="dim">${escapeHtml(clip.cannot_tell)}</p>
  ${clip.headphones ? '<p class="badge">Headphones</p>' : ''}
  <p class="verdict" aria-live="polite">○ Unheard</p>
  <div class="answers"><button type="button" class="good" data-verdict="good" aria-pressed="false">Good</button><button type="button" class="off" data-verdict="off" aria-pressed="false" aria-controls="detail-${index}" aria-expanded="false">Off</button></div>
  <div class="details" id="detail-${index}" hidden>
    <div class="chips reasons" role="group" aria-label="What sounds off?">${['Click or thump', 'Timing', 'Tone or level'].map((reason) => `<button type="button" aria-pressed="false">${reason}</button>`).join('')}</div>
    <label for="note-${index}">Note (optional)</label>
    <input id="note-${index}" class="note" type="text" maxlength="500" autocomplete="off">
  </div>
</article>`;
  }).join('\n');
  // JSON in a raw-text script needs escaping too, especially a literal closing script tag.
  const data = JSON.stringify(manifest).replace(/[<>&\u2028\u2029]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark">
<link rel="icon" href="data:,">
<title>BleepLoop: listening sheet</title>
<style>
:root { color-scheme:dark; --bg:#111211; --panel:#1b1d1b; --control:#292c29; --ink:#efede5; --dim:#b0b2a9; --good:#99c6a3; --off:#e2b773; }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--ink); font:16px/1.5 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }
main { max-width:640px; margin:auto; padding:28px 16px 48px; }
h1 { font-size:26px; line-height:1.2; margin:0 0 12px; }
h2 { font-size:20px; margin:36px 0 16px; }
h3 { font-size:16px; font-weight:500; margin:0; color:var(--dim); }
p { margin:12px 0; }
header .dim { margin:0 0 16px; }
.dim, .time, .badge { font-size:14px; color:var(--dim); }
.clip { background:var(--panel); padding:20px 16px; margin-bottom:16px; border-radius:8px; overflow-wrap:anywhere; }
.question { font-size:21px; line-height:1.4; margin:10px 0 20px; }
button, input, textarea { font:inherit; color:inherit; }
button { min-height:48px; min-width:48px; border:1px solid transparent; border-radius:6px; background:var(--control); padding:10px 14px; cursor:pointer; touch-action:manipulation; }
button:active { border-color:var(--ink); }
:focus-visible { outline:2px solid var(--ink); outline-offset:4px; }
.player { display:grid; grid-template-columns:80px minmax(0,1fr); gap:0 14px; align-items:center; }
.time { grid-column:2; text-align:right; font-variant-numeric:tabular-nums; }
.timeline { position:relative; min-width:0; }
.seek { display:block; width:100%; height:48px; margin:0; padding:0; accent-color:var(--ink); cursor:pointer; }
.ticks { position:absolute; inset:0 8px; pointer-events:none; }
.ticks span { position:absolute; top:30px; width:2px; height:8px; background:var(--dim); transform:translateX(-1px); }
.chips, .ab { display:flex; flex-wrap:wrap; gap:8px; margin-top:12px; }
.chips button { font-size:14px; text-align:left; }
.ab button { flex:1; }
.badge { display:inline-block; margin:0; border:1px solid var(--dim); border-radius:4px; padding:2px 8px; }
.answers { display:grid; grid-template-columns:1fr 1fr; gap:12px; }
.good { color:var(--good); } .off { color:var(--off); }
.good[aria-pressed="true"] { border-color:var(--good); }
.off[aria-pressed="true"] { border-color:var(--off); }
.reasons [aria-pressed="true"] { border-color:var(--off); color:var(--off); }
.verdict { font-size:16px; margin:16px 0 8px; }
.details label { display:block; margin:14px 0 6px; font-size:14px; }
.note, textarea { width:100%; min-height:48px; border:1px solid var(--dim); border-radius:6px; padding:10px; background:var(--bg); font-size:16px; }
footer { margin-top:32px; }
#copy { width:100%; font-weight:600; }
#export { margin-top:20px; } #export label { display:block; margin-bottom:8px; }
textarea { min-height:260px; resize:vertical; }
#clear { background:transparent; font-size:14px; color:var(--dim); margin-top:12px; }
[hidden] { display:none !important; }
.play-error:empty { display:none; }
.sr-only { position:absolute; width:1px; height:1px; padding:0; margin:-1px; overflow:hidden; clip-path:inset(50%); white-space:nowrap; }
@media (prefers-reduced-motion:reduce) { *, *::before, *::after { animation:none !important; transition:none !important; scroll-behavior:auto !important; } }
</style>
</head>
<body><main>
<header>
<h1>BleepLoop: listening sheet</h1>
<p class="dim">${escapeHtml(commit)} · ${escapeHtml(date)}</p>
<p>These are renders from the real engine with a synthetic plucked guitar; they settle clicks, the grid and curves, and say nothing about your guitar, latency, drivers, pedals or feel.</p>
<p id="progress" aria-live="polite">0 of ${clips.length} answered</p>
<p id="storage-status" class="dim" role="status"></p>
</header>
<noscript>Enable JavaScript to play clips and save answers.</noscript>
${cards}
<footer>
<button type="button" id="copy">Copy verdicts</button>
<div id="export" hidden><label for="verdict-text">Your verdicts</label><textarea id="verdict-text" readonly spellcheck="false"></textarea><p class="dim">Paste this to the agents.</p></div>
<p id="copy-status" class="dim" role="status"></p>
<button type="button" id="clear">Clear answers</button>
</footer>
</main>
<script id="sheet-data" type="application/json">${data}</script>
<script>
'use strict';
const sheet = JSON.parse(document.getElementById('sheet-data').textContent);
const key = 'bleeploop.listen.' + sheet.commit;
const reasons = ['Click or thump', 'Timing', 'Tone or level'];
let answers = Object.create(null);
const storageStatus = document.getElementById('storage-status');
try {
  const saved = JSON.parse(localStorage.getItem(key) || '{}');
  for (const clip of sheet.clips) {
    const value = saved && Object.hasOwn(saved, clip.id) ? saved[clip.id] : null;
    if (value && (value.verdict === 'good' || value.verdict === 'off')) {
      answers[clip.id] = {
        verdict: value.verdict,
        chips: reasons.filter((reason) => Array.isArray(value.chips) && value.chips.includes(reason)),
        note: typeof value.note === 'string' ? value.note.slice(0, 500) : '',
      };
    }
  }
} catch {
  storageStatus.textContent = 'Saved answers could not be loaded. Copy your verdicts before closing this page.';
}
const cards = Array.from(document.querySelectorAll('.clip'));
const audios = cards.map((card) => card.querySelector('audio'));
const verdictText = document.getElementById('verdict-text');
const exportBox = document.getElementById('export');
const copyStatus = document.getElementById('copy-status');
const timeLabel = (seconds) => Math.floor(seconds / 60) + ':' + String(Math.floor(seconds % 60)).padStart(2, '0');
function verdicts() {
  const lines = sheet.clips.map((clip) => {
    const answer = answers[clip.id];
    if (!answer) return clip.id + ': unheard';
    if (answer.verdict === 'good') return clip.id + ': good';
    return clip.id + ': off [' + answer.chips.join(', ') + '] ' + JSON.stringify(answer.note);
  });
  return ['BleepLoop listening sheet ' + sheet.commit + ' ' + sheet.date, ...lines].join('\\n');
}
function refresh() {
  let count = 0;
  cards.forEach((card, index) => {
    const answer = answers[sheet.clips[index].id];
    if (answer) count++;
    card.querySelector('.verdict').textContent = answer ? (answer.verdict === 'good' ? '✓ Good' : '! Off') : '○ Unheard';
    card.querySelectorAll('[data-verdict]').forEach((button) => {
      button.setAttribute('aria-pressed', String(answer?.verdict === button.dataset.verdict));
    });
    const off = answer?.verdict === 'off';
    card.querySelector('.off').setAttribute('aria-expanded', String(off));
    card.querySelector('.details').hidden = !off;
    card.querySelectorAll('.reasons button').forEach((button) => {
      button.setAttribute('aria-pressed', String(answer?.chips.includes(button.textContent) || false));
    });
    const note = card.querySelector('.note');
    if (note.value !== (answer?.note || '')) note.value = answer?.note || '';
  });
  document.getElementById('progress').textContent = count + ' of ' + sheet.clips.length + ' answered';
  if (!exportBox.hidden) verdictText.value = verdicts();
}
function save() {
  try { localStorage.setItem(key, JSON.stringify(answers)); storageStatus.textContent = ''; }
  catch { storageStatus.textContent = 'Answers cannot be saved here. Copy your verdicts before closing this page.'; }
  copyStatus.textContent = '';
  refresh();
}
cards.forEach((card, index) => {
  const clip = sheet.clips[index];
  const audio = audios[index];
  const play = card.querySelector('.play');
  const seek = card.querySelector('.seek');
  const error = card.querySelector('.play-error');
  let pendingSeek = null;
  function setPosition(seconds) {
    pendingSeek = seconds;
    if (audio.readyState > 0) { audio.currentTime = seconds; pendingSeek = null; }
    seek.value = String(seconds);
  }
  audio.addEventListener('loadedmetadata', () => {
    if (pendingSeek !== null) setPosition(pendingSeek);
  });
  function start(seconds) {
    audios.forEach((other) => { if (other !== audio) other.pause(); });
    error.textContent = '';
    if (seconds !== undefined) setPosition(seconds);
    else if (audio.ended) setPosition(0);
    audio.play().catch((failure) => {
      if (failure.name !== 'AbortError') error.textContent = 'Playback could not start. Tap Play to try again.';
    });
  }
  function updatePlayer() {
    const playing = !audio.paused && !audio.ended;
    play.textContent = playing ? 'Pause' : 'Play';
    play.setAttribute('aria-label', (playing ? 'Pause ' : 'Play ') + clip.title);
    const position = pendingSeek ?? audio.currentTime;
    seek.value = String(position);
    seek.setAttribute('aria-valuetext', timeLabel(position) + ' of ' + timeLabel(clip.seconds));
    card.querySelector('.time').textContent = timeLabel(position) + ' / ' + timeLabel(clip.seconds);
  }
  audio.addEventListener('play', () => {
    audios.forEach((other) => { if (other !== audio) other.pause(); });
    updatePlayer();
  });
  for (const event of ['pause', 'ended', 'timeupdate', 'seeked']) audio.addEventListener(event, updatePlayer);
  audio.addEventListener('error', () => { error.textContent = 'This clip could not be loaded by your browser.'; });
  play.addEventListener('click', () => { if (audio.paused) start(); else audio.pause(); });
  seek.addEventListener('input', () => { setPosition(Number(seek.value)); updatePlayer(); });
  card.querySelectorAll('[data-seek]').forEach((button) => {
    button.addEventListener('click', () => start(Number(button.dataset.seek)));
  });
  card.querySelectorAll('[data-verdict]').forEach((button) => {
    button.addEventListener('click', () => {
      answers[clip.id] = { chips: [], note: '', ...answers[clip.id], verdict: button.dataset.verdict };
      save();
    });
  });
  card.querySelectorAll('.reasons button').forEach((button) => {
    button.addEventListener('click', () => {
      const answer = answers[clip.id];
      const reason = button.textContent;
      answer.chips = answer.chips.includes(reason) ? answer.chips.filter((value) => value !== reason) : [...answer.chips, reason];
      save();
    });
  });
  card.querySelector('.note').addEventListener('input', (event) => {
    answers[clip.id].note = event.target.value;
    save();
  });
  updatePlayer();
});
document.getElementById('copy').addEventListener('click', async () => {
  const text = verdicts();
  exportBox.hidden = false;
  verdictText.value = text;
  copyStatus.textContent = 'Tap the text to select it, then copy.';
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      copyStatus.textContent = 'Copied. The text is also available below.';
    }
  } catch { copyStatus.textContent = 'Tap the text to select it, then copy.'; }
});
verdictText.addEventListener('click', () => { verdictText.select(); verdictText.setSelectionRange(0, verdictText.value.length); });
document.getElementById('clear').addEventListener('click', () => {
  if (!window.confirm('Clear all answers for this listening sheet?')) return;
  answers = Object.create(null);
  save();
});
refresh();
</script>
</body></html>`;
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length && !(args.length === 2 && args[0] === '--from' && args[1])) {
    throw new Error('Usage: node scripts/listen.mjs [--from <folder>]');
  }
  const folder = args.length ? resolve(args[1]) : join(output, 'clips');
  if (!args.length) await render(folder);
  const { clips, audio } = await readClips(folder);
  const git = await promisify(execFile)('git', ['rev-parse', '--short', 'HEAD'], { cwd: root });
  const manifest = { commit: git.stdout.trim(), date: new Date().toISOString().slice(0, 10), clips };
  const html = page(manifest, audio);
  await mkdir(output, { recursive: true });
  await writeFile(join(output, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
  const path = join(output, 'index.html');
  await writeFile(path, html);
  console.log(`${path}\n${clips.length} clips, ${(Buffer.byteLength(html) / 1_000_000).toFixed(2)} MB`);
}

main().catch((error) => {
  console.error(`Listening sheet: ${error.message}`);
  process.exitCode = 1;
});
