/* MASKIN 2 control sheet: the drawn parts, copied from main.js (icons, glyphs, knob, fader, meters, OTT
   miniature), plus the lane heads in every state and the four displays. */
(function () {
  const ICON = {
    save: '<path d="M4 3h8l3 3v9H4z"/><path d="M7 3v4h5V3M7 15v-4h5v4"/>',
    open: '<path d="M3 6h4l1.5 1.5H15V14H3z"/><path d="M3 6V4h4"/>',
    keys: '<rect x="2.5" y="5" width="13" height="8" rx="1"/><path d="M6 5v5M9 5v5M12 5v5"/>',
    stage: '<rect x="2.5" y="3.5" width="13" height="9" rx="1"/><path d="M6 15.5h6M9 12.5v3"/>',
    tuner: '<path d="M3.5 13a6 6 0 0 1 11 0"/><path d="M9 13l2.6-5"/><path d="M9 4.5v1.5"/>',
    help: '<circle cx="9" cy="9" r="6.5"/><path d="M7 7.3a2 2 0 1 1 2.6 1.9c-.5.2-.6.6-.6 1.2"/><path d="M9 12.6v.2"/>',
    gear: '<circle cx="9" cy="9" r="2.3"/><path d="M9 2.5v2M9 13.5v2M2.5 9h2M13.5 9h2M4.4 4.4l1.4 1.4M12.2 12.2l1.4 1.4M4.4 13.6l1.4-1.4M12.2 5.8l1.4-1.4"/>',
    win: '<rect x="2.5" y="5" width="9.5" height="9.5" rx="1"/><path d="M9 2.5h6.5V9M15.5 2.5L9.5 8.5"/>',
    drop: '<path d="M5 7l4 4 4-4"/>',
    plus: '<path d="M9 3.5v11M3.5 9h11"/>',
    // the sheet's own: the plate's menu
    params: '<path d="M3 5h12M3 9h12M3 13h12"/><circle cx="6.5" cy="5" r="1.6" fill="#1e2125"/><circle cx="11.5" cy="9" r="1.6" fill="#1e2125"/><circle cx="8" cy="13" r="1.6" fill="#1e2125"/>',
    left: '<path d="M14.5 9h-11M7.5 5 3.5 9l4 4"/>',
    right: '<path d="M3.5 9h11M10.5 5l4 4-4 4"/>',
    x: '<path d="M4.5 4.5l9 9M13.5 4.5l-9 9"/>'
  };
  const icon = (n, s) => `<svg width="${s}" height="${s}" viewBox="0 0 18 18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${ICON[n]}</svg>`;
  const X = s => `<svg width="${s}" height="${s}" viewBox="0 0 9 9" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M1 1l7 7M8 1L1 8"/></svg>`;

  /* ---------- 1. the lane head in every state ---------- */
  const G = {
    rec: '<circle cx="12" cy="12" r="10"/><circle class="fill" cx="12" cy="12" r="4.6"/>',
    dub: '<circle cx="12" cy="12" r="10"/><path d="M12 7.2v9.6M7.2 12h9.6"/>',
    play: '<path d="M7.6 4.6 19.6 12 7.6 19.4z"/>'
  };
  const glyph = k => `<svg class="g" viewBox="0 0 24 24">${G[k]}</svg>`;
  const LO = { lead: 1, on: .92, low: .42, empty: 0 }, GL = { lead: 1, on: .45, low: .15, empty: 0 };
  const SC = { lead: c => c, on: c => `color-mix(in srgb, ${c} 88%, #15171b)`, low: () => '#7a8189', empty: () => '#5f666e' };
  // win: plain (one bar) | rec (rec lamp + bar) | count (four beat cells, `count` still lit) | fade (light runs out to the right)
  const S = [
    { n: 5, st: 'EMPTY', c: '#5d646c', next: 'rec', t: 'empty', empty: true, cap: 'next press: <em>record</em>' },
    { n: 5, st: 'COUNT-IN', c: '#ff3b52', t: 'lead', win: 'count', count: 3, cap: 'counting in: <em>3</em> beats to the take' },
    { n: 5, st: 'REC', c: '#ff3b52', next: 'dub', t: 'lead', win: 'rec', cap: 'next press: <em>overdub</em>' },
    { n: 3, st: 'OVERDUB', c: '#ffae2a', next: 'play', t: 'lead', win: 'rec', from: 'A', sel: true, cap: 'next press: <em>play</em> · the selected lane' },
    { n: 1, st: 'PLAYING', c: '#45d295', next: 'dub', t: 'on', cap: 'next press: <em>overdub</em>' },
    { n: 2, st: 'FADING', c: '#45d295', next: 'dub', t: 'on', win: 'fade', cap: 'fading out over 2 bars · next: <em>overdub</em>' },
    { n: 1, st: 'STOPPED', c: '#aab1b9', next: 'play', t: 'on', cap: 'next press: <em>play</em>' },
    { n: 4, st: 'MUTED', c: '#7a8189', next: 'dub', t: 'low', cap: 'next press: <em>overdub</em>' },
    { n: 1, st: 'PLAYING', c: '#45d295', next: 'dub', t: 'on', held: true, cap: 'the key held down' },
    { n: 1, st: 'PLAYING', c: '#45d295', next: 'dub', t: 'on', foc: true, cap: 'keyboard focus' }
  ];
  const windowOf = l => {
    if (l.win === 'rec') return '<div class="lw split"><i class="seg dot"></i><i class="seg"></i></div>';
    if (l.win === 'count') return `<div class="lw split">${[0, 1, 2, 3].map(i => `<i class="seg ${i < l.count ? '' : 'off'}"></i>`).join('')}</div>`;
    if (l.win === 'fade') return '<div class="lw fade"></div>';
    return '<div class="lw"></div>';
  };
  const face = l => l.win === 'count' ? `<span class="cnt">${l.count}</span>` : glyph(l.next);
  const mainKey = l => `<div class="ka ${l.empty ? 'unlit' : 'lit'} ${l.held ? 'held' : ''} ${l.foc ? 'foc' : ''}" style="--c:${l.c};--lo:${LO[l.t]};--gl:${GL[l.t]}">${windowOf(l)}${face(l)}</div>`;
  const ident = l => `<div class="ident"><span class="num lnum">${l.n}</span><span class="st">${l.st}</span>${l.from ? `<span class="lg from">FROM ${l.from}</span>` : ''}</div>`;
  const head = (l, side) => {
    const dis = l.empty ? 'dis' : '';
    return `<div class="cell head lanecell t-${l.t} ${l.sel ? 'sel' : ''}" style="--sc:${SC[l.t](l.c)}">${ident(l)}${mainKey(l)}${side ? `<div class="side"><button class="key stop ${dis}"><span class="sqg"></span>STOP</button><button class="key clr ${dis}">CLR</button></div>` : ''}</div>`;
  };
  document.getElementById('lanes').innerHTML = S.map(l => `<div class="it">${head(l, true)}<span class="sc">${l.cap}</span></div>`).join('');
  document.getElementById('gs').innerHTML = S.slice(0, 8).map(l => `<div class="cell head t-${l.t}" style="--sc:${SC[l.t](l.c)}">${ident(l)}${mainKey(l)}</div>`).join('');

  /* ---------- small marks ---------- */
  document.querySelectorAll('[data-x]').forEach(e => e.innerHTML = X(9) + 'ALL');
  document.querySelectorAll('[data-xl]').forEach(e => e.innerHTML = X(9));
  document.querySelectorAll('[data-edit]').forEach(e => e.insertAdjacentHTML('beforeend', icon('drop', 12)));
  document.querySelectorAll('svg.echo').forEach(s => s.innerHTML = '<g fill="#d4d8dd"><rect x="7" y="4" width="2" height="18" rx="1"/><rect x="19" y="9" width="2" height="13" rx="1"/><rect x="31" y="13" width="2" height="9" rx="1"/><rect x="43" y="16" width="2" height="6" rx="1"/></g>');
  document.getElementById('platemenu').innerHTML =
    `<div class="mi">${icon('win', 16)}Editor</div><div class="mi hl">${icon('params', 16)}Parameters</div><div class="sep"></div>` +
    `<div class="mi">${icon('left', 16)}Move left</div><div class="mi">${icon('right', 16)}Move right</div><div class="sep"></div>` +
    `<div class="mi rm">${icon('x', 16)}Remove</div>`;

  /* ---------- 8. tools ---------- */
  document.querySelectorAll('[data-tools]').forEach(el => {
    const [k, i] = (el.dataset.tools || ':').split(':');
    el.innerHTML = ['save', 'open', 'keys', 'stage', 'tuner', 'help', 'gear'].map((n, j) => `<span class="${String(j) === i ? k : ''}">${icon(n, 18)}</span>`).join('');
  });

  /* ---------- 7. the display, four moments ---------- */
  const lock = '<svg width="9" height="11" viewBox="0 0 9 11" fill="none" stroke="#8d939b" stroke-width="1.3"><rect x="1" y="5" width="7" height="5.3" rx="1" fill="#8d939b" stroke="none"/><path d="M2.6 5V3.4a1.9 1.9 0 0 1 3.8 0V5"/></svg>';
  const disp = o => `<div class="it l"><div class="bezel disp"><div class="glass">
      <span class="num tempo ${o.locked ? '' : 'ed'}">120</span>
      <div class="meta num"><span class="top">BPM${o.locked ? lock : ''}</span><span>4/4</span></div>
      <div class="lamps">${[0, 1, 2, 3].map(i => `<i class="${i === o.beat ? o.lamp : ''}"></i>`).join('')}</div>
      <div class="ro"><span class="cap lg">LOOP</span><span class="fig num ${o.loop ? '' : 'dash'}">${o.loop || '—'}${o.loop ? `<span class="w lg">${o.loop === 1 ? 'BAR' : 'BARS'}</span>` : ''}</span></div>
      <div class="ro"><span class="cap lg">${o.cap2 || 'BAR.BEAT'}</span><span class="fig num ${o.fig2c || ''}">${o.fig2}</span></div>
      ${o.prog ? `<div class="progress" style="--w:${o.prog}">${o.ticks.map(t => `<b style="left:${t}%"></b>`).join('')}</div>` : '<div class="progress empty"></div>'}
    </div></div><span class="sc">${o.sc}</span></div>`;
  document.getElementById('disps').innerHTML = [
    disp({ locked: true, beat: 1, lamp: 'on', loop: 4, fig2: '2.2', prog: 1, ticks: [0, 25, 50, 75], sc: 'as on the main screen: loops exist, the tempo locked' }),
    disp({ locked: false, beat: 1, lamp: 'on', loop: 0, fig2: '—', fig2c: 'dash', sc: 'no loops yet: no padlock, the tempo takes a drag, a scroll or TAP' }),
    disp({ locked: false, beat: 1, lamp: 'rc', loop: 0, cap2: 'COUNT-IN', fig2: '3', fig2c: 'rc', sc: 'counting in: the beat lamps count in red, the take starts after 1' }),
    disp({ locked: true, beat: 0, lamp: 'on', loop: 1, fig2: '1.2', prog: 1, ticks: [0], sc: 'a 1-bar loop: no bar ticks inside the line' })
  ].join('');

  /* ---------- drawn controls (from main.js) ---------- */
  document.querySelectorAll('[data-icon]').forEach(e => e.insertAdjacentHTML('afterbegin', icon(e.dataset.icon, +e.dataset.isz || 18)));

  document.querySelectorAll('.knob').forEach(el => {
    const s = parseFloat(el.style.getPropertyValue('--s')) || 40, v = +el.dataset.v, n = +(el.dataset.ticks || 11);
    el.style.setProperty('--a', (-135 + 270 * v) + 'deg');
    el.style.setProperty('--kd', (360 * 1.35 / (Math.PI * s)).toFixed(3) + 'deg');
    const tl = Math.max(3.5, s * .08), gap = 3, pad = gap + tl + 4, W = s + pad * 2, c = W / 2, r0 = s / 2 + gap, r1 = r0 + tl;
    let t = '';
    for (let i = 0; i < n; i++) {
      const a = (-135 + 270 * i / (n - 1)) * Math.PI / 180, major = i === 0 || i === n - 1;
      t += `<line x1="${c + r0 * Math.sin(a)}" y1="${c - r0 * Math.cos(a)}" x2="${c + (r1 + (major ? 1 : 0)) * Math.sin(a)}" y2="${c - (r1 + (major ? 1 : 0)) * Math.cos(a)}" stroke="${major ? '#8f96a0' : '#555c65'}" stroke-width="1.3" stroke-linecap="round"/>`;
    }
    if (el.dataset.centre != null) t += `<line x1="${c}" y1="${c - r0 + .5}" x2="${c}" y2="${c - r1 - 2}" stroke="#dfe3e8" stroke-width="1.6" stroke-linecap="round"/>`;
    el.innerHTML = `<svg class="ticks" width="${W}" height="${W}">${t}</svg><div class="body"></div><div class="cap"></div><div class="ptr"></div>`;
  });

  document.querySelectorAll('.fader').forEach(el => {
    const w = el.offsetWidth, g = Math.pow(10, +el.dataset.db / 20) * 100, inset = 15, span = w - inset * 2, X = m => (inset + m / 150 * span).toFixed(1);
    let h = '<div class="rail"></div>';
    for (const m of [0, 25, 50, 75, 125, 150]) h += `<i class="tick" style="left:${X(m)}px"></i>`;
    h += `<i class="tick unity" style="left:${X(100)}px"></i><span class="num zero" style="left:${X(100)}px">0</span><div class="capf" style="left:${X(Math.min(150, g))}px"></div>`;
    el.innerHTML = h;
  });

  const xp = db => db <= -24 ? (db + 54) / 30 * 0.36 : db <= -12 ? 0.36 + (db + 24) / 12 * 0.22 : 0.58 + (db + 12) / 15 * 0.42;
  const segs = [];
  for (let db = -54; db < 3; db += db < -24 ? 3 : db < -12 ? 1.5 : 1) segs.push(db);
  document.querySelectorAll('.meter .lad').forEach(l => {
    const lit = +l.dataset.lit, pk = +l.dataset.pk;
    segs.forEach((db, i) => {
      const nx = i + 1 < segs.length ? segs[i + 1] : 3, s = document.createElement('i');
      s.className = db < -12 ? 'g' : db < -3 ? 'y' : 'r';
      if (nx <= lit + 0.01 || (db <= pk && nx > pk)) s.classList.add('on');
      const a = xp(db), b = xp(nx);
      s.style.left = `calc(${a * 100}% + 1px)`; s.style.width = `calc(${(b - a) * 100}% - 2px)`;
      l.appendChild(s);
    });
  });
  document.querySelectorAll('.meter .scale').forEach(sc => [-48, -24, -12, -6, 0].forEach(db => {
    const p = xp(db) * 100;
    sc.insertAdjacentHTML('beforeend', `<b style="left:${p}%"></b><span style="left:${p}%">${db < 0 ? '−' + (-db) : db}</span>`);
  }));

  document.querySelectorAll('.sm').forEach(el => {
    const lit = +el.dataset.lit, pk = +el.dataset.pk, n = 24, pitch = 3;
    let h = '';
    for (let r = 0; r < 2; r++) for (let i = 0; i < n; i++) {
      const db = -36 + i * 1.5, on = db < lit - r * .8 || (db <= pk && db + 1.5 > pk);
      h += `<i class="${db < -12 ? 'g' : 'y'} ${on ? 'on' : ''}" style="left:${i * pitch}px;top:${1 + r * 6}px"></i>`;
    }
    el.innerHTML = h;
  });

  document.querySelectorAll('svg.ott').forEach(s => {
    let h = '';
    [[7, 3, 2], [21, 2, 3], [35, 3, 1]].forEach(([x, up, dn]) => {
      for (let k = 0; k < 6; k++) {
        const y = 3.5 + k * 3.4, on = k < 3 ? k >= 3 - up : k < 3 + dn;
        h += `<rect x="${x}" y="${y.toFixed(1)}" width="10" height="2" rx=".6" fill="${on ? (k < 3 ? '#fff1d8' : '#ffa630') : '#202226'}"/>`;
      }
    });
    s.innerHTML = h;
  });
})();
