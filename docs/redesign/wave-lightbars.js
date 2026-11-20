/* MASKIN 2 main screen: builds the drawn parts and the five lanes, paints the glass.
   main.html?alt swaps the lane's main button (lit key -> state pad) and the wave (light bars -> neutral). */
if (/[?&]alt\b/.test(location.search)) document.body.classList.add('alt');
(function () {
  const ALT = document.body.classList.contains('alt');
  const PH = 0.3125; // bar 2, beat 2 of a 4-bar loop

  /* ---------- icons: one 1.6 px stroke on an 18 px box ---------- */
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
    plus: '<path d="M9 3.5v11M3.5 9h11"/>'
  };
  const icon = (n, s) => `<svg width="${s}" height="${s}" viewBox="0 0 18 18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${ICON[n]}</svg>`;
  document.getElementById('tools').innerHTML = ['save', 'open', 'keys', 'stage', 'tuner', 'help', 'gear'].map(n => `<span>${icon(n, 18)}</span>`).join('');

  /* ---------- next-action glyphs (line drawings) ---------- */
  const G = {
    rec: '<circle cx="12" cy="12" r="8.6"/>',
    dub: '<circle cx="12" cy="12" r="10"/><path d="M12 7.2v9.6M7.2 12h9.6"/>',
    play: '<path d="M7.6 4.6 19.6 12 7.6 19.4z"/>'
  };
  const glyph = (k, s) => `<svg class="g" width="${s}" height="${s}" viewBox="0 0 24 24">${G[k]}</svg>`;

  /* ---------- the five lanes ---------- */
  const L = [
    { n: 1, st: 'PLAYING', c: '#45d295', next: 'dub', w: ['strum', 3], db: 0, pan: 'C', pv: .5 },
    { n: 2, st: 'PLAYING', c: '#45d295', next: 'dub', w: ['chug', 4], db: -2.5, pan: 'L20', pv: .4, fx: true },
    { n: 3, st: 'OVERDUB', c: '#ffae2a', next: 'play', w: ['pick', 5], db: 0, pan: 'C', pv: .5, sel: true, from: 'A' },
    { n: 4, st: 'MUTED', c: '#7a8189', next: 'dub', w: ['lead', 6], db: -4, pan: 'R15', pv: .575, mute: true },
    { n: 5, st: 'EMPTY', c: '#5d646c', next: 'rec', db: 0, pan: 'C', pv: .5, empty: true }
  ];
  const fmtDb = d => (d > 0 ? '+' : d < 0 ? '−' : '+') + Math.abs(d).toFixed(1);

  function mainA(l) {
    const lit = !l.empty;
    return `<div class="ka ${lit ? 'lit' : 'unlit'} ${l.st === 'MUTED' ? 'dim' : ''}" style="--c:${l.c};--lo:${l.st === 'MUTED' ? .5 : 1}"><div class="lw"></div>${glyph(l.next, 24)}</div>`;
  }
  function mainB(l) {
    const lit = ['REC', 'OVERDUB', 'PLAYING'].includes(l.st);
    const smoke = { STOPPED: ['#2b3037', '#181b1f', '#eef1f4'], MUTED: ['#202328', '#131518', '#8a919a'], EMPTY: ['#1c1f23', '#111316', '#585e66'] }[l.st];
    const v = lit ? `--c:${l.c}` : `--smoke-hi:${smoke[0]};--smoke-lo:${smoke[1]};--pn:${smoke[2]}`;
    return `<div class="pb ${lit ? 'lit' : 'unlit'}" style="${v}"><div class="face"><span class="pn">${l.n}</span><span class="cg">${glyph(l.next, 16)}</span></div></div>`;
  }

  const loops = document.getElementById('loops');
  let html = '';
  L.forEach((l, i) => {
    const cls = `${l.sel ? 'sel' : ''} ${i ? 'g' : ''}`;
    const row = `grid-row:${i + 1}`;
    const litWord = ['PLAYING', 'OVERDUB', 'REC'].includes(l.st);
    const ident = `<div class="ident">${ALT ? '' : `<span class="num lnum" ${l.empty ? 'style="color:#4c525a"' : l.mute ? 'style="color:#959ba3"' : ''}>${l.n}</span>`}
      <span class="st ${litWord ? 'lit' : ''}" style="--c:${l.c}">${l.st}</span>${l.from ? `<span class="lg from">FROM ${l.from}</span>` : ''}</div>`;
    const dis = l.empty ? 'dis' : '';
    html += `<div class="cell head ${cls}" style="grid-column:1;${row}">${ident}${ALT ? mainB(l) : mainA(l)}
      <div class="side"><button class="key stop ${dis}"><span class="sqg"></span>STOP</button><button class="key clr ${dis}">CLR</button></div></div>`;
    html += `<div class="cell mix ${cls}" style="grid-column:3;${row}">
      <div class="mkeys">
        <button class="key latch fx ${dis}"><span class="led ${l.fx ? 'on' : ''}"></span>FX</button>
        <button class="key latch mute ${dis}"><span class="led ${l.mute ? 'on' : ''}"></span>MUTE</button>
        <span class="sp"></span>
        <button class="key k3 ${dis}">REV</button><button class="key k3 ${dis}">UNDO</button><button class="key k3 ${dis}">EDIT${icon('drop', 13)}</button>
      </div>
      <div class="mline">
        <div class="fader ${dis}" data-db="${l.db}"></div>
        <span class="win num ${dis}"><span>${fmtDb(l.db)}<span class="u">dB</span></span><span>${l.pan}</span></span>
        <div class="knob ${dis}" style="--s:34px" data-v="${l.pv}" data-ticks="11" data-centre></div>
      </div></div>`;
  });
  loops.insertAdjacentHTML('beforeend', html);

  /* ---------- drawn controls ---------- */
  document.querySelectorAll('[data-icon]').forEach(e => e.insertAdjacentHTML('afterbegin', icon(e.dataset.icon, +e.dataset.isz || 18)));

  // knob: printed ticks round the body, -135..+135 deg; data-centre marks 12 o'clock
  document.querySelectorAll('.knob').forEach(el => {
    const s = parseFloat(el.style.getPropertyValue('--s')) || 40, v = +el.dataset.v, n = +(el.dataset.ticks || 11);
    el.style.setProperty('--a', (-135 + 270 * v) + 'deg');
    el.style.setProperty('--kd', (360 * 1.35 / (Math.PI * s)).toFixed(3) + 'deg');
    el.style.setProperty('--pw', (s < 34 ? 2 : 2) + 'px');
    const tl = Math.max(3.5, s * .08), gap = 3, pad = gap + tl + 4, W = s + pad * 2, c = W / 2, r0 = s / 2 + gap, r1 = r0 + tl;
    let t = '';
    for (let i = 0; i < n; i++) {
      const a = (-135 + 270 * i / (n - 1)) * Math.PI / 180, major = i === 0 || i === n - 1;
      t += `<line x1="${c + r0 * Math.sin(a)}" y1="${c - r0 * Math.cos(a)}" x2="${c + (r1 + (major ? 1 : 0)) * Math.sin(a)}" y2="${c - (r1 + (major ? 1 : 0)) * Math.cos(a)}" stroke="${major ? '#8f96a0' : '#555c65'}" stroke-width="1.3" stroke-linecap="round"/>`;
    }
    if (el.dataset.centre != null) t += `<line x1="${c}" y1="${c - r0 + .5}" x2="${c}" y2="${c - r1 - 2}" stroke="#dfe3e8" stroke-width="1.6" stroke-linecap="round"/>`;
    el.innerHTML = `<svg class="ticks" width="${W}" height="${W}">${t}</svg><div class="body"></div><div class="cap"></div><div class="ptr"></div>`;
  });

  // fader: 0..150 % gain, unity (100 %) at 2/3, the scale printed under the slot
  document.querySelectorAll('.fader').forEach(el => {
    const w = el.offsetWidth, g = Math.pow(10, +el.dataset.db / 20) * 100, inset = 15, span = w - inset * 2, X = m => (inset + m / 150 * span).toFixed(1);
    let h = '<div class="slot"></div>';
    for (const m of [0, 25, 50, 75, 125, 150]) h += `<i class="tick" style="left:${X(m)}px"></i>`;
    h += `<i class="tick unity" style="left:${X(100)}px"></i><span class="num zero" style="left:${X(100)}px">0</span><div class="capf" style="left:${X(g)}px"></div>`;
    el.innerHTML = h;
  });

  // master meter: -54..+3 dB on a piecewise scale so the top 12 dB get room; green < -12, amber -12..-3, red > -3
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

  // slot meters: a thin stereo ladder, 32 segments, -48..0 dB
  document.querySelectorAll('.sm').forEach(el => {
    const lit = +el.dataset.lit, pk = +el.dataset.pk, n = 32, pitch = 3;
    let h = '';
    for (let r = 0; r < 2; r++) for (let i = 0; i < n; i++) {
      const db = -48 + i * 1.5, on = db < lit - r * .8 || (db <= pk && db + 1.5 > pk);
      h += `<i class="${db < -12 ? 'g' : 'y'} ${on ? 'on' : ''}" style="left:${i * pitch}px;top:${1 + r * 6}px"></i>`;
    }
    el.innerHTML = h;
  });

  // OTT miniature: three bands, each a short ladder: lift above the centre (warm white), cut below (amber)
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

  /* ---------- the glass ---------- */
  const COL = { play: [69, 210, 149], dub: [255, 174, 42], muted: [104, 110, 118], white: [244, 238, 228], whiteMuted: [96, 101, 108] };
  const rgba = (c, a) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
  const mk = (style, seed) => BLWave.make(style, { bars: 4, cols: 3200, seed });
  const W = L.map(l => l.empty ? null : mk(l.w[0], l.w[1]));
  const NEW = mk('lead', 8); // the layer being laid over lane 3 now
  const SUM = (() => {
    const a = W[2], o = { cols: a.cols, peak: new Float32Array(a.cols), rms: new Float32Array(a.cols) };
    for (let c = 0; c < a.cols; c++) { o.peak[c] = Math.min(1, Math.hypot(a.peak[c], NEW.peak[c] * .82)); o.rms[c] = Math.min(o.peak[c], Math.hypot(a.rms[c], NEW.rms[c] * .82)); }
    return o;
  })();
  const NEWS = { cols: NEW.cols, peak: NEW.peak.map(v => v * .82), rms: NEW.rms.map(v => v * .82) };

  function env(d, w, key) {
    const out = new Float32Array(w);
    for (let i = 0; i < w; i++) {
      const c0 = Math.floor(i / w * d.cols), c1 = Math.max(c0 + 1, Math.floor((i + 1) / w * d.cols));
      let m = 0, s = 0;
      for (let c = c0; c < c1; c++) { const v = d[key][Math.min(d.cols - 1, c)]; if (v > m) m = v; s += v; }
      out[i] = key === 'peak' ? m : s / (c1 - c0);
    }
    return out;
  }
  function smooth(a, sg) {
    const r = Math.ceil(sg * 3), k = []; let sum = 0;
    for (let i = -r; i <= r; i++) { const v = Math.exp(-(i * i) / (2 * sg * sg)); k.push(v); sum += v; }
    const out = new Float32Array(a.length);
    for (let i = 0; i < a.length; i++) { let s = 0; for (let j = -r; j <= r; j++) s += a[Math.min(a.length - 1, Math.max(0, i + j))] * k[j + r]; out[i] = s / sum; }
    return out;
  }
  function shape(d, w, sg) {
    const p = env(d, w, 'peak'), q = new Float32Array(w);
    for (let i = 0; i < w; i++) q[i] = Math.max(p[i], p[i - 1] || 0, p[i + 1] || 0);
    return { pk: smooth(q, sg), rm: smooth(env(d, w, 'rms'), sg) };
  }
  function body(g, a, amp, mid, x0, x1) {
    g.beginPath(); g.moveTo(x0, mid - a[x0] * amp);
    for (let x = x0 + 1; x < x1; x++) g.lineTo(x + .5, mid - a[x] * amp);
    for (let x = x1 - 1; x >= x0; x--) g.lineTo(x + .5, mid + a[x] * amp);
    g.closePath();
  }
  function edge(g, a, amp, mid, x0, x1, sign) { g.beginPath(); for (let x = x0; x < x1; x++) g[x === x0 ? 'moveTo' : 'lineTo'](x + .5, mid - sign * a[x] * amp); }
  function clipX(g, x0, x1, h, fn) { g.save(); g.beginPath(); g.rect(x0, 0, x1 - x0, h); g.clip(); fn(); g.restore(); }
  const stateOf = l => l.st === 'OVERDUB' ? 'dub' : l.st === 'MUTED' ? 'muted' : 'play';

  // B. light bars: a 3 px rounded bar on a 5 px pitch; the dim peak with the bright RMS inside it
  function paintB(g, l, d, w, h, ph) {
    const st = stateOf(l), mid = h / 2, amp = h * (st === 'muted' ? .22 : .34), P = 5, n = Math.floor(w / P);
    const fit = dd => { const sub = { cols: Math.round(dd.cols * n * P / w), peak: dd.peak, rms: dd.rms }; return sub; };
    const pkOf = dd => env(fit(dd), n, 'peak'), rmOf = dd => env(fit(dd), n, 'rms');
    g.lineCap = 'round'; g.lineWidth = 3;
    const bar = (x, p, r, outer, inner) => {
      const hp = Math.max(1.5, p * amp), hr = Math.max(0, r * amp);
      g.strokeStyle = outer; g.beginPath(); g.moveTo(x, mid - hp + 1.5); g.lineTo(x, mid + hp - 1.5); g.stroke();
      if (hr > 1.5) { g.strokeStyle = inner; g.beginPath(); g.moveTo(x, mid - hr + 1.5); g.lineTo(x, mid + hr - 1.5); g.stroke(); }
    };
    const p = pkOf(d), r = rmOf(d);
    if (st === 'muted') { for (let j = 0; j < n; j++) bar(j * P + 2.5, p[j], r[j], rgba(COL.muted, .2), rgba(COL.muted, .42)); return; }
    const c = COL[st], dub = st === 'dub', sp = dub ? pkOf(SUM) : p, sr = dub ? rmOf(SUM) : r;
    for (let j = 0; j < n; j++) {
      const x = j * P + 2.5, played = x < ph;
      if (dub) played ? bar(x, sp[j], sr[j], rgba(c, .44), rgba([255, 200, 104], 1)) : bar(x, p[j], r[j], rgba(c, .26), rgba(c, .64));
      else played ? bar(x, p[j], r[j], rgba(c, .28), rgba(c, .74)) : bar(x, p[j], r[j], rgba(c, .2), rgba(c, .56));
    }
  }
  // C. neutral display: the display's own white light; amber only where lane 3 is being written
  function paintC(g, l, d, w, h, ph) {
    const st = stateOf(l), mid = h / 2, amp = h * (st === 'muted' ? .22 : .32), sg = 2.2;
    const lit = (s, c, kb, kc, ke, x0 = 0, x1 = w) => {
      body(g, s.pk, amp, mid, x0, x1); g.fillStyle = rgba(c, kb); g.fill();
      body(g, s.rm, amp, mid, x0, x1); g.fillStyle = rgba(c, kc); g.fill();
      g.lineWidth = 1; g.strokeStyle = rgba(c, ke); edge(g, s.pk, amp, mid, x0, x1, 1); g.stroke(); edge(g, s.pk, amp, mid, x0, x1, -1); g.stroke();
    };
    const s = shape(d, w, sg);
    if (st === 'muted') { lit(s, COL.whiteMuted, .10, .30, .26); return; }
    clipX(g, 0, ph, h, () => lit(s, COL.white, .16, .74, .5));
    clipX(g, ph, w, h, () => lit(s, COL.white, .10, .5, .36));
    if (st === 'dub') {
      const nw = shape(NEWS, w, sg), x = Math.round(ph);
      clipX(g, 0, x, h, () => {
        body(g, nw.pk, amp, mid, 0, w); g.fillStyle = 'rgba(2,2,3,0.7)'; g.fill();
        body(g, nw.pk, amp, mid, 0, w); g.fillStyle = rgba(COL.dub, .5); g.fill();
        body(g, nw.rm, amp, mid, 0, w); g.fillStyle = rgba([255, 186, 74], 1); g.fill();
      });
    }
  }

  const pane = document.getElementById('pane'), glass = pane.querySelector('.glass'), cv = document.getElementById('cv');
  const gw = glass.clientWidth, LH = 182, gh = LH * 5, dpr = window.devicePixelRatio || 1;
  cv.style.width = gw + 'px'; cv.style.height = gh + 'px'; cv.width = Math.round(gw * dpr); cv.height = Math.round(gh * dpr);
  // the glass inner edge sits exactly on the lanes' top: the bezel reaches 4 px above and below (main.css)
  const g = cv.getContext('2d'); g.scale(dpr, dpr);
  const ph = Math.round(PH * gw);
  L.forEach((l, i) => {
    const y = i * LH;
    g.save(); g.translate(0, y); g.beginPath(); g.rect(0, 0, gw, LH); g.clip();
    if (l.sel) { g.fillStyle = 'rgba(255,255,255,0.02)'; g.fillRect(0, 0, gw, LH); }
    for (let b = 1; b < 16; b++) { const x = Math.round(b / 16 * gw); g.fillStyle = b % 4 === 0 ? 'rgba(255,255,255,0.075)' : 'rgba(255,255,255,0.026)'; g.fillRect(x, 0, 1, LH); }
    if (!l.empty) { g.fillStyle = 'rgba(255,255,255,0.03)'; g.fillRect(0, Math.floor(LH / 2), gw, 1); (ALT ? paintC : paintB)(g, l, W[i], gw, LH, ph); }
    g.restore();
    if (i) { g.fillStyle = 'rgba(0,0,0,0.95)'; g.fillRect(0, y, gw, 1); g.fillStyle = 'rgba(255,255,255,0.04)'; g.fillRect(0, y + 1, gw, 1); }
  });
  g.fillStyle = 'rgba(255,243,224,0.95)'; g.fillRect(ph, 0, 1, gh);
  g.beginPath(); g.moveTo(ph - 4, 0); g.lineTo(ph + 5, 0); g.lineTo(ph + .5, 6); g.closePath(); g.fill();
  for (let b = 0; b < 4; b++) glass.insertAdjacentHTML('beforeend', `<span class="bn" style="left:${Math.round(b / 4 * gw) + 6}px">${b + 1}</span>`);
})();
