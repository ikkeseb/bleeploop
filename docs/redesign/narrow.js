/* MASKIN 2 main screen, narrow (1000x700): builds the drawn parts and the five lanes, paints the glass.
   Copied from main.js; the WAVE block is main.js's, unchanged. */
(function () {
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
    plus: '<path d="M9 3.5v11M3.5 9h11"/>',
    menu: '<path d="M3 5h12M3 9h12M3 13h12"/>'
  };
  const icon = (n, s) => `<svg width="${s}" height="${s}" viewBox="0 0 18 18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">${ICON[n]}</svg>`;

  /* ---------- next-action glyphs: ring + dot (record), ring + plus (overdub), triangle (play) ---------- */
  const G = {
    rec: '<circle cx="12" cy="12" r="10"/><circle class="fill" cx="12" cy="12" r="4.6"/>',
    dub: '<circle cx="12" cy="12" r="10"/><path d="M12 7.2v9.6M7.2 12h9.6"/>',
    play: '<path d="M7.6 4.6 19.6 12 7.6 19.4z"/>'
  };
  const glyph = k => `<svg class="g" viewBox="0 0 24 24">${G[k]}</svg>`;

  /* ---------- the five lanes. t: lead (the selected lane) > on > low (muted) > empty ---------- */
  const L = [
    { n: 1, st: 'PLAYING', c: '#45d295', next: 'dub', w: ['strum', 3], db: 0, pan: 'C', pv: .5, t: 'on' },
    { n: 2, st: 'PLAYING', c: '#45d295', next: 'dub', w: ['chug', 4], db: -2.5, pan: 'L20', pv: .4, fx: true, t: 'on' },
    { n: 3, st: 'OVERDUB', c: '#ffae2a', next: 'play', w: ['pick', 5], db: 0, pan: 'C', pv: .5, sel: true, from: 'A', t: 'lead' },
    { n: 4, st: 'MUTED', c: '#7a8189', next: 'dub', w: ['lead', 6], db: -4, pan: 'R15', pv: .575, mute: true, t: 'low' },
    { n: 5, st: 'EMPTY', c: '#5d646c', next: 'rec', db: 0, pan: 'C', pv: .5, empty: true, t: 'empty' }
  ];
  const LO = { lead: 1, on: .92, low: .42, empty: 0 }, GL = { lead: 1, on: .45, low: .15, empty: 0 }; // window strength, bloom
  const SC = { lead: c => c, on: c => `color-mix(in srgb, ${c} 88%, #15171b)`, low: () => '#7a8189', empty: () => '#5f666e' };
  const fmtDb = d => (d > 0 ? '+' : d < 0 ? '−' : '+') + Math.abs(d).toFixed(1);

  const mainKey = l => `<div class="ka ${l.empty ? 'unlit' : 'lit'}" style="--c:${l.c};--lo:${LO[l.t]};--gl:${GL[l.t]}">${/^(REC|OVERDUB)$/.test(l.st) ? '<div class="lw split"><i class="seg dot"></i><i class="seg"></i></div>' : '<div class="lw"></div>'}${glyph(l.next)}</div>`;

  const loops = document.getElementById('loops');
  let html = '';
  L.forEach((l, i) => {
    const cls = `t-${l.t} ${l.sel ? 'sel' : ''} ${i ? 'g' : ''}`;
    const row = `grid-row:${i + 1}`;
    const dis = l.empty ? 'dis' : '';
    html += `<div class="cell head ${cls}" style="grid-column:1;${row};--sc:${SC[l.t](l.c)}">
      <div class="ident"><span class="num lnum">${l.n}</span><span class="st">${l.st}</span>${l.from ? `<span class="lg from">FROM ${l.from}</span>` : ''}</div>
      ${mainKey(l)}
      <div class="side"><button class="key stop ${dis}"><span class="sqg"></span>STOP</button><button class="key clr ${dis}">CLR</button></div></div>`;
    html += `<div class="cell mix ${cls}" style="grid-column:3;${row}">
      <div class="mkeys">
        <button class="key latch fx ${dis}"><span class="led ${l.fx ? 'on' : ''}"></span>FX</button>
        <button class="key latch mute ${dis}"><span class="led ${l.mute ? 'on' : ''}"></span>MUTE</button>
        <span class="sp"></span>
        <button class="key mo ${dis}" title="Reverse · Undo · Edit">EDIT${icon('drop', 12)}</button>
      </div>
      <div class="mline">
        <div class="fader ${dis}" data-db="${l.db}"></div>
        <span class="win num ${dis}"><span class="lv">${fmtDb(l.db)}<span class="u">dB</span></span><span class="pn">${l.pan}</span></span>
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
    const tl = Math.max(3.5, s * .08), gap = 3, pad = gap + tl + 4, W = s + pad * 2, c = W / 2, r0 = s / 2 + gap, r1 = r0 + tl;
    let t = '';
    for (let i = 0; i < n; i++) {
      const a = (-135 + 270 * i / (n - 1)) * Math.PI / 180, major = i === 0 || i === n - 1;
      t += `<line x1="${c + r0 * Math.sin(a)}" y1="${c - r0 * Math.cos(a)}" x2="${c + (r1 + (major ? 1 : 0)) * Math.sin(a)}" y2="${c - (r1 + (major ? 1 : 0)) * Math.cos(a)}" stroke="${major ? '#8f96a0' : '#555c65'}" stroke-width="1.3" stroke-linecap="round"/>`;
    }
    if (el.dataset.centre != null) t += `<line x1="${c}" y1="${c - r0 + .5}" x2="${c}" y2="${c - r1 - 2}" stroke="#dfe3e8" stroke-width="1.6" stroke-linecap="round"/>`;
    el.innerHTML = `<svg class="ticks" width="${W}" height="${W}">${t}</svg><div class="body"></div><div class="cap"></div><div class="ptr"></div>`;
  });

  // fader: 0..150 % gain, unity (100 %) at 2/3, the scale printed under the rail
  document.querySelectorAll('.fader').forEach(el => {
    const w = el.offsetWidth, g = Math.pow(10, +el.dataset.db / 20) * 100, inset = 15, span = w - inset * 2, X = m => (inset + m / 150 * span).toFixed(1);
    let h = '<div class="rail"></div>';
    for (const m of [0, 25, 50, 75, 125, 150]) h += `<i class="tick" style="left:${X(m)}px"></i>`;
    h += `<i class="tick unity" style="left:${X(100)}px"></i><span class="num zero" style="left:${X(100)}px">0</span><div class="capf" style="left:${X(g)}px"></div>`;
    el.innerHTML = h;
  });

  // master meter: -54..+3 dB on a piecewise scale so the top 12 dB get room; green < -12, amber -12..-3, red > -3
  // short master meter: -36..+3 dB, the top 15 dB get 55 % of the width; green < -12, amber -12..-3, red > -3
  const xp = db => db <= -12 ? (db + 36) / 24 * 0.45 : 0.45 + (db + 12) / 15 * 0.55;
  const segs = [];
  for (let db = -36; db < 3; db += db < -12 ? 3 : 1) segs.push(db);
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
  document.querySelectorAll('.meter .scale').forEach(sc => [-24, -12, -6, 0].forEach(db => {
    const p = xp(db) * 100;
    sc.insertAdjacentHTML('beforeend', `<b style="left:${p}%"></b><span style="left:${p}%">${db < 0 ? '−' + (-db) : db}</span>`);
  }));

  // slot meters: a thin stereo ladder, 16 segments, -36..0 dB
  document.querySelectorAll('.sm').forEach(el => {
    const lit = +el.dataset.lit, pk = +el.dataset.pk, n = 16, pitch = 3;
    let h = '';
    for (let r = 0; r < 2; r++) for (let i = 0; i < n; i++) {
      const db = -36 + i * 2.25, on = db < lit - r * .8 || (db <= pk && db + 2.25 > pk);
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
  /* WAVE BEGIN */
  // The data and colours every take reads:
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
  const stateOf = l => l.st === 'OVERDUB' ? 'dub' : l.st === 'MUTED' ? 'muted' : 'play';
  // Three takes on "the wave is light on dark glass", each paint(g, l, d, w, h, ph): g is translated to the
  // lane's top left and clipped to it, w x h the lane on the glass in CSS px, ph the playhead's x.
  // Shared language: the part already played is lit, the part to come is the same light turned down;
  // a short trail behind the playhead burns brightest at the playhead (it reads in a still);
  // lane 3's new layer is the hottest light, laid over the existing one; MUTED keeps its shape in a
  // cool grey with a lighter core, never flat grey on grey.
  const { paintA, paintB, paintC } = (() => {
    const cache = new WeakMap();
    // n columns: the peak is the max, the rms the mean of the source columns under each; then smoothed
    // (peak: a neighbour max, then a gaussian of sg columns) so no column stands alone. Cached per data.
    function cols(d, n, sg) {
      let m = cache.get(d); if (!m) cache.set(d, m = new Map());
      const key = n + '/' + sg; if (m.has(key)) return m.get(key);
      const pk = new Float32Array(n), rm = new Float32Array(n);
      for (let i = 0; i < n; i++) {
        const c0 = Math.floor(i / n * d.cols), c1 = Math.max(c0 + 1, Math.floor((i + 1) / n * d.cols));
        let a = 0, s = 0;
        for (let c = c0; c < c1; c++) { const v = d.peak[Math.min(d.cols - 1, c)]; if (v > a) a = v; s += d.rms[Math.min(d.cols - 1, c)]; }
        pk[i] = a; rm[i] = s / (c1 - c0);
      }
      const nb = pk.map((v, i) => Math.max(v, pk[i - 1] || 0, pk[i + 1] || 0));
      const r = { pk: gauss(nb, sg), rm: gauss(rm, sg) };
      for (let i = 0; i < n; i++) r.rm[i] = Math.min(r.rm[i], r.pk[i]);
      r.halo = r.rm.map((v, i) => v + (r.pk[i] - v) * .45); // a soft step between core and body
      m.set(key, r); return r;
    }
    function gauss(a, sg) {
      if (!sg) return Float32Array.from(a);
      const r = Math.ceil(sg * 3), k = []; let sum = 0;
      for (let i = -r; i <= r; i++) { const v = Math.exp(-(i * i) / (2 * sg * sg)); k.push(v); sum += v; }
      const out = new Float32Array(a.length);
      for (let i = 0; i < a.length; i++) { let s = 0; for (let j = -r; j <= r; j++) s += a[Math.min(a.length - 1, Math.max(0, i + j))] * k[j + r]; out[i] = s / sum; }
      return out;
    }
    const mix = (c, t) => c.map(v => Math.round(v + (255 - v) * t));
    const GREY = [138, 145, 154]; // MUTED's core: the state's grey, one step lighter so the shape still reads
    // the light a column gets: played 1, to come `ahead`, plus a trail that ramps up to the playhead
    const TRAIL = 84;
    const light = (x, ph, ahead) => x >= ph ? ahead : 1;
    const trail = (x, ph) => x < ph && x > ph - TRAIL ? Math.pow(1 - (ph - x) / TRAIL, 2.2) : 0;
    function rr(g, x, y, w, h, r) { // a rounded bar, radius clamped to the bar
      r = Math.min(r, w / 2, h / 2);
      g.moveTo(x + r, y); g.arcTo(x + w, y, x + w, y + h, r); g.arcTo(x + w, y + h, x, y + h, r);
      g.arcTo(x, y + h, x, y, r); g.arcTo(x, y, x + w, y, r); g.closePath();
    }
    // a vertical gradient over the lane, brightest on the centre line
    function vg(g, h, c, aEdge, aMid) {
      const gr = g.createLinearGradient(0, 0, 0, h);
      gr.addColorStop(0, rgba(c, aEdge)); gr.addColorStop(.5, rgba(c, aMid)); gr.addColorStop(1, rgba(c, aEdge));
      return gr;
    }

    // A. Fine and dense: 2 px rounded bars on a 3 px pitch, the envelope smoothed. Each bar carries its
    // own light: a bright core over the rms that falls off smoothly to a soft dim tip at the peak, so the
    // lane reads as one finely ribbed body of light, brightest on the centre line, with no terraces.
    // On lane 3 the new layer is the white-hot core, the existing take the amber round it.
    function paintA(g, l, d, w, h, ph) {
      const st = stateOf(l), mu = st === 'muted', dub = st === 'dub', c = mu ? GREY : COL[st];
      const P = 3, BW = 2, n = Math.floor(w / P), mid = h / 2, amp = (h / 2 - 16) * (mu ? .9 : 1);
      const base = cols(d, n, 1.4), top = dub ? cols(SUM, n, 1.4) : base, nu = dub ? cols(NEWS, n, 1.4) : null;
      const ahead = mu ? .8 : .5, cc = mix(c, mu ? 0 : .16), hot = mix(COL.dub, .62), white = mix(c, .78);
      // stops: [fraction of the half-height from the centre (0) to the tip (1), colour, alpha]
      const bar = (x, hh, stops) => {
        const gr = g.createLinearGradient(0, mid - hh, 0, mid + hh);
        for (let i = stops.length - 1; i >= 0; i--) { const [f, col, a] = stops[i]; gr.addColorStop((1 - f) / 2, rgba(col, a)); }
        for (const [f, col, a] of stops) if (f > 0) gr.addColorStop((1 + f) / 2, rgba(col, a));
        g.fillStyle = gr; g.beginPath(); rr(g, x, mid - hh, BW, hh * 2, 1); g.fill();
      };
      for (let j = 0; j < n; j++) {
        const x = j * P, played = x + BW <= ph, src = played ? top : base, p = src.pk[j];
        const hh = Math.max(1, p * amp), r = Math.min(.96, base.rm[j] / Math.max(1e-3, p));
        const t = mu ? 0 : trail(x + BW, ph);
        g.globalAlpha = played ? 1 : ahead;
        if (mu) bar(x, hh, [[0, c, .42], [r, c, .34], [1, c, .1]]);
        else if (dub && played) {
          const rn = Math.min(.9, nu.rm[j] / Math.max(1e-3, p));
          bar(x, hh, [[0, mix(hot, t * .5), 1], [rn, hot, .92], [Math.min(.97, Math.max(r, rn) + .1), c, .55], [1, c, .16]]);
        } else bar(x, hh, [[0, t ? mix(cc, t * .7) : cc, 1], [r, cc, .82 + t * .18], [1, c, .14 + t * .2]]);
        if (t > .02 && !mu) { g.globalAlpha = t * .55; const hw = Math.max(1, (dub ? nu.rm[j] : base.rm[j]) * amp * .8); g.fillStyle = rgba(white, 1); g.beginPath(); rr(g, x, mid - hw, BW, hw * 2, 1); g.fill(); }
      }
      g.globalAlpha = 1;
    }

    // B. LED matrix: each column a ladder of 4x2 px lamps on a 6x4 pitch, mirrored on the centre line,
    // the cells' brightness falling from the centre to the peak; the top cell lit by its fraction so the
    // edge stays smooth. The unlit cells show faintly, like the panel's meters.
    function paintB(g, l, d, w, h, ph) {
      const st = stateOf(l), mu = st === 'muted', dub = st === 'dub', c = mu ? GREY : COL[st];
      const PX = 6, CW = 4, PY = 4, CH = 2, n = Math.floor(w / PX), mid = Math.round(h / 2);
      const rows = Math.floor((h / 2 - 16) / PY), ahead = mu ? .8 : .55;
      const base = cols(d, n, 1.1), top = dub ? cols(SUM, n, 1.1) : base, nu = dub ? cols(NEWS, n, 1.1) : null;
      const cell = (x, k) => { g.fillRect(x, mid + 1 + k * PY, CW, CH); g.fillRect(x, mid - 1 - k * PY - CH, CW, CH); };
      g.fillStyle = 'rgba(255,255,255,0.014)'; // the unlit lamps, as on the panel's meters
      for (let j = 0; j < n; j++) for (let k = 0; k < rows; k++) cell(j * PX + 1, k);
      const hot = mix(COL.dub, .55), cc = mix(c, mu ? 0 : .18);
      for (let j = 0; j < n; j++) {
        const x = j * PX + 1, played = x < ph, pk = (played ? top : base).pk[j] * rows, rm = base.rm[j] * rows;
        const nr = played && dub ? nu.rm[j] * rows : 0, L = light(x, ph, ahead), t = mu ? 0 : trail(x + CW, ph);
        const kmax = Math.min(rows, Math.ceil(pk));
        for (let k = 0; k < kmax; k++) {
          const f = Math.min(1, pk - k); // the top cell lit by its fraction
          const inCore = k < rm, inNew = k < nr;
          // brightness falls smoothly from the centre to the rms (1 -> .78), then on to the peak (-> .16)
          let a = inCore || inNew ? 1 - .22 * k / Math.max(1, rm) : .78 - .62 * (k - rm + .5) / Math.max(1, pk - rm);
          a *= f * L * (mu ? .5 : 1.12);
          g.fillStyle = rgba(inNew ? hot : inCore ? cc : c, Math.min(1, a)); cell(x, k);
          if (t > .05 && (inCore || inNew)) { g.fillStyle = rgba(mix(inNew ? hot : c, .8), t * .7 * f); cell(x, k); } // the trail
        }
      }
    }

    // C. From the floor: a level history. 3 px bars on a 4 px pitch rise from a baseline near the lane's
    // floor over its full height (a gentle curve so quiet notes still stand). Each bar is lit from its top:
    // a bright tip, the light falling through the rms to a dim foot, so the lane reads as a lit skyline;
    // a faint reflection under the floor. On lane 3 the new layer stacks on top of the existing take.
    function paintC(g, l, d, w, h, ph) {
      const st = stateOf(l), mu = st === 'muted', dub = st === 'dub', c = mu ? GREY : COL[st];
      const P = 4, BW = 3, n = Math.floor(w / P), floor = h - 15, H = floor - 20, ahead = mu ? .8 : .5;
      const lv = v => Math.pow(Math.min(1, v), .85) * H;
      const base = cols(d, n, 1.2), top = dub ? cols(SUM, n, 1.2) : base;
      const tip = mix(c, mu ? .05 : .4), hot = mix(COL.dub, .6), white = mix(c, .8);
      const k = mu ? .36 : 1;
      // a bar from y0 (its foot) up to y1 (its tip), lit from the tip down: [fraction from the tip, colour, alpha]
      const bar = (x, y0, y1, stops) => {
        if (y0 - y1 < .5) return;
        const gr = g.createLinearGradient(0, y1, 0, y0);
        for (const [f, col, a] of stops) gr.addColorStop(Math.max(0, Math.min(1, f)), rgba(col, a));
        g.fillStyle = gr; g.beginPath(); rr(g, x, y1, BW, y0 - y1, 1.5); g.fill();
      };
      const refl = g.createLinearGradient(0, floor + 1, 0, h);
      refl.addColorStop(0, rgba(c, mu ? .07 : .14)); refl.addColorStop(1, rgba(c, 0));
      for (let j = 0; j < n; j++) {
        const x = j * P, played = x + BW <= ph, t = mu ? 0 : trail(x + BW, ph);
        const ex = Math.max(2, lv(base.pk[j])), rm = Math.max(1, lv(base.rm[j])), r = 1 - rm / ex;
        g.globalAlpha = played ? 1 : ahead;
        const tipC = t ? mix(tip, t * .7) : tip;
        if (dub && played) {
          const all = Math.max(ex + 2, lv(top.pk[j]));
          bar(x, floor, floor - ex, [[0, c, .62], [r, c, .48], [1, c, .14]]); // the existing take, under
          bar(x, floor - ex - 1, floor - all, [[0, t ? mix(hot, t * .6) : hot, 1], [.6, hot, .9], [1, mix(COL.dub, .25), .72]]); // the new layer, on top
        } else bar(x, floor, floor - ex, [[0, tipC, .95 * k], [4 / ex, c, .78 * k + t * .2], [r, c, .5 * k + t * .2], [1, c, .1 * k]]);
        g.fillStyle = refl; g.fillRect(x, floor + 2, BW, Math.min(h - floor - 2, ex * .09));
        if (t > .05) { g.globalAlpha = t; g.fillStyle = rgba(white, .9); g.beginPath(); rr(g, x, floor - Math.max(ex, dub ? lv(top.pk[j]) : 0), BW, 2, 1); g.fill(); }
      }
      g.globalAlpha = 1;
    }
    return { paintA, paintB, paintC };
  })();
  // ---- F, G, H: stripes on the musical grid. A whole power of two of stripes per bar (the one whose pitch
  // lands nearest TP css px), so every bar and beat line falls in a gap and a stripe is a slice of the bar.
  // A stripe is the peak (max) of its slice, square-ended, mirrored on the centre line, snapped to device
  // pixels; a silent slice keeps a 2 px tick. Flat fills only: no glow, no blur, no trail. The part to come
  // is the same stripe turned down; the stripe under the playhead counts as played.
  const { paintF, paintG, paintH, paintM } = (() => {
    const DPR = window.devicePixelRatio || 1, TP = 8.5, DUTY = .46, BARS = 4;
    const mix = (c, t, to = [255, 255, 255]) => c.map((v, i) => Math.round(v + (to[i] - v) * t));
    const GLASS = [7, 9, 11];
    const GREY = [86, 92, 101]; // MUTED: a cool grey a step above the muted label, so the shape stays legible
    const HOT = [255, 236, 200]; // lane 3's new layer: the hot warm white
    const cache = new WeakMap();
    function slices(d, N) { // per stripe: pk the max of the slice's peaks, rm the mean of its rms
      let m = cache.get(d); if (!m) cache.set(d, m = new Map());
      if (m.has(N)) return m.get(N);
      const pk = new Float32Array(N), rm = new Float32Array(N);
      for (let i = 0; i < N; i++) {
        const c0 = Math.floor(i / N * d.cols), c1 = Math.max(c0 + 1, Math.floor((i + 1) / N * d.cols));
        let a = 0, s = 0;
        for (let c = c0; c < c1; c++) { a = Math.max(a, d.peak[c]); s += d.rms[c]; }
        pk[i] = a; rm[i] = Math.min(a, s / (c1 - c0));
      }
      const r = { pk, rm }; m.set(N, r); return r;
    }
    // the grid: n stripes per bar, pitch p, stripe width sw and the lane's centre/amplitude, all in device px
    function grid(w, h) {
      const bar = w / BARS; let n = 8;
      for (const k of [16, 32, 64]) if (Math.abs(bar / k - TP) < Math.abs(bar / n - TP)) n = k;
      const N = n * BARS, p = w / N * DPR;
      return { N, p, sw: Math.max(1, Math.round(p * DUTY)), mid: Math.round(h / 2 * DPR), amp: (h / 2 - 24) * DPR, tick: Math.max(1, Math.round(DPR)) };
    }
    const G0 = new WeakMap();
    // walk the stripes: f(x, half(v), played, j) with x the stripe's left edge in device px
    function each(G, ph, f) {
      for (let j = 0; j < G.N; j++) {
        const x = Math.round((j + .5) * G.p - G.sw / 2);
        f(x, v => Math.max(G.tick, Math.round(v * G.amp)), (j + .5) * G.p / DPR < ph, j);
      }
    }
    // one mirrored stripe from half-height a to b (device px from the centre line): two rects, or one through the centre
    function band(g, G, x, a, b, style) {
      if (b <= a) return;
      g.fillStyle = style;
      if (a === 0) g.fillRect(x / DPR, (G.mid - b) / DPR, G.sw / DPR, 2 * b / DPR);
      else { g.fillRect(x / DPR, (G.mid - b) / DPR, G.sw / DPR, (b - a) / DPR); g.fillRect(x / DPR, (G.mid + a) / DPR, G.sw / DPR, (b - a) / DPR); }
    }
    // the turned-down copy of a colour: mixed toward the glass, so it is a solid, not a see-through
    const down = (c, t) => mix(c, t, GLASS);
    const css = c => `rgb(${c[0]},${c[1]},${c[2]})`;
    function tones(l) {
      const st = stateOf(l);
      if (st === 'muted') return { st, on: GREY, ahead: down(GREY, .34) };
      if (st === 'dub') return { st, on: COL.dub, ahead: down(COL.dub, .55), hot: HOT };
      return { st, on: mix(COL.play, .04), ahead: down(COL.play, .58) };
    }

    // F. Flat stripes, the reference in BleepLoop's colours: one solid fill per stripe.
    // PLAYING: played in the state green, to come the same green turned down. OVERDUB, played: the new
    // layer's peak as a hot warm-white stripe from the centre, the old take's amber round it out to the
    // sum; to come, the existing loop's amber turned down. MUTED: one cool grey, to come a step lower.
    function paintF(g, l, d, w, h, ph) {
      const G = grid(w, h), T = tones(l), base = slices(d, G.N);
      const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
      each(G, ph, (x, H, played, j) => {
        if (sum && played) {
          const all = H(sum.pk[j]), core = Math.min(H(nu.rm[j] * 1.25), all - 3 * G.tick);
          band(g, G, x, 0, all, css(T.on)); band(g, G, x, 0, core, css(T.hot));
        } else band(g, G, x, 0, H(base.pk[j]), css(played ? T.on : T.ahead));
      });
    }

    // G. The same stripes, each filled with one vertical gradient over the lane: full colour on the centre
    // line, easing toward the glass as it nears the tallest reach, so a quiet stripe stays bright and a
    // loud one tapers. (The reverse, bright tips, was tried: it sank the quiet stripes into the dark.)
    function lg(g, G, c, k) { // c on the centre line, c turned down by k at the reach of the tallest stripe
      const gr = g.createLinearGradient(0, (G.mid - G.amp) / DPR, 0, (G.mid + G.amp) / DPR);
      const m = css(c), e = css(down(c, k));
      gr.addColorStop(0, e); gr.addColorStop(.5, m); gr.addColorStop(1, e);
      return gr;
    }
    function paintG(g, l, d, w, h, ph) {
      const G = grid(w, h), T = tones(l), base = slices(d, G.N), K = .42;
      const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
      const on = lg(g, G, T.on, K), ahead = lg(g, G, T.ahead, K), hot = sum && lg(g, G, T.hot, .16);
      each(G, ph, (x, H, played, j) => {
        if (sum && played) {
          const all = H(sum.pk[j]), core = Math.min(H(nu.rm[j] * 1.25), all - 3 * G.tick);
          band(g, G, x, 0, all, on); band(g, G, x, 0, core, hot);
        } else band(g, G, x, 0, H(base.pk[j]), played ? on : ahead);
      });
    }

    // M. G with the gradient the owner asked for: smoother and softer. Same stripes; the colour eases from
    // full on the centre line to K down at the tallest reach along a smoothstep, in 24 stops (no band).
    function lgs(g, G, c, k) {
      const gr = g.createLinearGradient(0, (G.mid - G.amp) / DPR, 0, (G.mid + G.amp) / DPR), n = 24;
      for (let i = 0; i <= n; i++) { const t = i / n, u = Math.abs(t - .5) * 2, e = u * u * (3 - 2 * u); gr.addColorStop(t, css(down(c, k * e))); }
      return gr;
    }
    function paintM(g, l, d, w, h, ph) {
      const G = grid(w, h), T = tones(l), base = slices(d, G.N), K = .66;
      const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
      const on = lgs(g, G, T.on, K), ahead = lgs(g, G, T.ahead, K), hot = sum && lgs(g, G, T.hot, .22);
      each(G, ph, (x, H, played, j) => {
        if (sum && played) {
          const all = H(sum.pk[j]), core = Math.min(H(nu.rm[j] * 1.25), all - 3 * G.tick);
          band(g, G, x, 0, all, on); band(g, G, x, 0, core, hot);
        } else band(g, G, x, 0, H(base.pk[j]), played ? on : ahead);
      });
    }

    // H. Two tones: the slice's rms solid from the centre, its peak above it the same colour turned
    // down, so the loudness and the attack both read in one flat stripe. OVERDUB, played: the new
    // layer's rms hot warm white at the centre, then the old take's amber to the sum's rms, then the
    // turned-down amber to the sum's peak. To come: one flat silhouette, turned down, as in F.
    function paintH(g, l, d, w, h, ph) {
      const G = grid(w, h), T = tones(l), base = slices(d, G.N);
      const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
      const capOf = c => down(c, T.st === 'muted' ? .3 : .34); // a step down, still above the part to come
      const on = css(T.on), onCap = css(capOf(T.on)), ah = css(T.ahead);
      each(G, ph, (x, H, played, j) => {
        const s = sum && played ? sum : base, pk = H(s.pk[j]), rm = Math.min(pk, Math.max(G.tick, H(s.rm[j] * 1.15)));
        if (!played) { band(g, G, x, 0, pk, ah); return; } // to come: one flat turned-down silhouette
        band(g, G, x, rm, pk, onCap); band(g, G, x, 0, rm, on);
        if (sum) band(g, G, x, 0, Math.min(rm - 3 * G.tick, H(nu.rm[j] * .9)), css(T.hot));
      });
    }
    return { paintF, paintG, paintH, paintM };
  })();

  // ---- J, K, L: F's stripes, finer, with a soft gradient. A whole multiple of 4 stripes per bar (the one
  // whose pitch lands nearest TP css px), so bar and beat lines fall in gaps. Stripe left edges snap to
  // device pixels (crisp sides); the tips keep their sub-pixel height, so the canvas anti-aliases them and
  // neighbours step smoothly. Height: the slice's peak blended with a gaussian of its neighbours' peaks
  // (sigma ~1 stripe), so the outline reads as one drawn shape and an attack still stands up.
  const { paintJ, paintK, paintL } = (() => {
    const DPR = window.devicePixelRatio || 1, BARS = 4;
    const GLASS = [7, 9, 11], GREY = [96, 103, 113], HOT = [255, 236, 200];
    const mix = (c, t, to) => c.map((v, i) => v + (to[i] - v) * t);
    const css = (c, a = 1) => `rgba(${c.map(Math.round).join(',')},${a})`;
    const ease = t => t <= 0 ? 0 : t >= 1 ? 1 : t * t * (3 - 2 * t); // smoothstep: flat at both ends, no knee
    const cache = new WeakMap();
    const RAW = .35, KER = [.11, .24, .30, .24, .11]; // the blend: 35 % own peak, 65 % a gaussian (sigma ~1.3 stripes)
    function slices(d, N) {
      let m = cache.get(d); if (!m) cache.set(d, m = new Map());
      if (m.has(N)) return m.get(N);
      const pk = new Float32Array(N), rm = new Float32Array(N), v = new Float32Array(N);
      for (let i = 0; i < N; i++) {
        const c0 = Math.floor(i / N * d.cols), c1 = Math.max(c0 + 1, Math.floor((i + 1) / N * d.cols));
        let a = 0, s = 0;
        for (let c = c0; c < c1; c++) { a = Math.max(a, d.peak[c]); s += d.rms[c]; }
        pk[i] = a; rm[i] = Math.min(a, s / (c1 - c0));
      }
      for (let i = 0; i < N; i++) {
        let s = 0; for (let k = -2; k <= 2; k++) s += KER[k + 2] * pk[(i + k + N) % N]; // the loop wraps
        v[i] = RAW * pk[i] + (1 - RAW) * s;
      }
      const r = { pk, rm, v }; m.set(N, r); return r;
    }
    function grid(w, h, TP, DUTY) {
      const bar = w / BARS, n = Math.max(4, 4 * Math.round(bar / (4 * TP))), N = n * BARS, p = w / N * DPR;
      return { N, n, p, sw: Math.max(1, Math.round(p * DUTY)), mid: Math.round(h / 2 * DPR), amp: (h / 2 - 22) * DPR, tick: Math.max(1, Math.round(DPR)) };
    }
    function each(G, ph, f) {
      for (let j = 0; j < G.N; j++) {
        const x = Math.round((j + .5) * G.p - G.sw / 2);
        f(x, v => Math.max(G.tick, v * G.amp), (j + .5) * G.p / DPR < ph, j);
      }
    }
    const rect = (g, G, x, b, style) => { g.fillStyle = style; g.fillRect(x / DPR, (G.mid - b) / DPR, G.sw / DPR, 2 * b / DPR); };
    function tones(l, lift = 0) {
      const st = stateOf(l), up = c => mix(c, lift, [255, 255, 255]);
      if (st === 'muted') return { st, on: up(GREY), ahead: mix(up(GREY), .36 - lift * .5, GLASS) };
      if (st === 'dub') return { st, on: up(COL.dub), ahead: mix(COL.dub, .55 - lift * .5, GLASS), hot: HOT };
      return { st, on: mix(COL.play, .06 + lift, [255, 255, 255]), ahead: mix(COL.play, .58 - lift * .5, GLASS) };
    }
    // the lane's vertical gradient: c on the centre line, eased (smoothstep) to c turned down by k at the
    // tallest stripe's reach; 25 stops so the canvas's linear interpolation between them never shows a knee
    function vg(g, G, c, k) {
      const R = G.amp * .8, gr = g.createLinearGradient(0, (G.mid - R) / DPR, 0, (G.mid + R) / DPR);
      for (let i = 0; i <= 24; i++) { const u = i / 24, t = Math.abs(u * 2 - 1); gr.addColorStop(u, css(mix(c, k * ease(t), GLASS))); }
      return gr;
    }
    function stripes(TP, DUTY, K, LIFT = 0) { // LIFT: a step toward white, for stripes too thin to carry the colour
      return function (g, l, d, w, h, ph) {
        const G = grid(w, h, TP, DUTY), T = tones(l, LIFT), base = slices(d, G.N);
        const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
        const on = vg(g, G, T.on, K), ahead = vg(g, G, T.ahead, K * .8), hot = sum && vg(g, G, T.hot, .3);
        each(G, ph, (x, H, played, j) => {
          if (sum && played) {
            const all = H(sum.v[j]), core = Math.min(H(nu.rm[j] * 1.25), all - 3 * G.tick);
            rect(g, G, x, all, on); if (core > 0) rect(g, G, x, core, hot);
          } else rect(g, G, x, H(base.v[j]), played ? on : ahead);
        });
      };
    }
    // J. ~64 stripes per bar at 1176 px (a 4.6 px pitch, 2 px stripes), the soft lane gradient.
    const paintJ = stripes(4.6, .44, .68);
    // K. ~96 per bar (a 3.06 px pitch, 1 px stripes at dpr 1, 2-3 device px above), the same gradient.
    const paintK = stripes(3.06, .42, .68, .08);
    // L. J's geometry; each stripe fades toward its OWN tip: full over its inner quarter, then eased to
    // nearly gone, so the silhouette has a soft edge. The new layer's core fades less, so it stays on top.
    function tip(g, G, x, b, c, keep, floor) {
      const gr = g.createLinearGradient(0, (G.mid - b) / DPR, 0, (G.mid + b) / DPR);
      for (let i = 0; i <= 16; i++) { const u = i / 16, t = Math.abs(u * 2 - 1); gr.addColorStop(u, css(c, 1 - (1 - floor) * ease((t - keep) / (1 - keep)))); }
      rect(g, G, x, b, gr);
    }
    function paintL(g, l, d, w, h, ph) {
      const G = grid(w, h, 4.6, .44), T = tones(l), base = slices(d, G.N);
      const sum = T.st === 'dub' ? slices(SUM, G.N) : null, nu = sum && slices(NEWS, G.N);
      each(G, ph, (x, H, played, j) => {
        if (sum && played) {
          const all = H(sum.v[j]), core = Math.min(H(nu.rm[j] * 1.25), all - 3 * G.tick);
          tip(g, G, x, all, T.on, .25, .14); if (core > 0) tip(g, G, x, core, T.hot, .45, .35);
        } else tip(g, G, x, H(base.v[j]), played ? T.on : T.ahead, .25, .14);
      });
    }
    return { paintJ, paintK, paintL };
  })();

  // ---- S. The plain wave: what a good hardware sampler's display draws. One flat, filled body per region,
  // mirrored about the centre line, with an exact anti-aliased edge; no stripes, gradient, core or glow.
  // Contour: one sample per device pixel, the peak (max) of the source columns under it, then a 3-tap
  // neighbour max (keeps a one-pixel attack from being smoothed away), then a gaussian (sigma SG css px)
  // so the edge is smooth, never jagged; drawn as one path of lineTo points, so the canvas anti-aliases it.
  // Uses from the header only: COL, SUM, NEWS, stateOf.
  const paintS = (() => {
    const DPR = window.devicePixelRatio || 1, SG = 2.2, AIR = 38, GLASS = [7, 9, 11];
    const mix = (c, t, to) => c.map((v, i) => v + (to[i] - v) * t);
    const css = c => `rgb(${c.map(Math.round).join(',')})`;
    const cache = new WeakMap();
    function env(d, n) {
      let m = cache.get(d); if (!m) cache.set(d, m = new Map());
      if (m.has(n)) return m.get(n);
      const pk = new Float32Array(n), dl = new Float32Array(n), v = new Float32Array(n);
      for (let i = 0; i < n; i++) {
        const c0 = Math.floor(i / n * d.cols), c1 = Math.max(c0 + 1, Math.floor((i + 1) / n * d.cols));
        let a = 0; for (let c = c0; c < c1; c++) a = Math.max(a, d.peak[c]);
        pk[i] = a;
      }
      for (let i = 0; i < n; i++) dl[i] = Math.max(pk[(i - 1 + n) % n], pk[i], pk[(i + 1) % n]);
      const s = SG * DPR, R = Math.ceil(3 * s), K = [];
      let sum = 0; for (let k = -R; k <= R; k++) { const e = Math.exp(-k * k / (2 * s * s)); K.push(e); sum += e; }
      for (let i = 0; i < n; i++) { let a = 0; for (let k = -R; k <= R; k++) a += K[k + R] * dl[(i + k + n) % n]; v[i] = a / sum; }
      m.set(n, v); return v;
    }
    // the mirrored body of half-heights hv (css px) over columns [i0, i1), filled flat
    function body(g, hv, i0, i1, mid, fill) {
      const x = i => i / DPR;
      g.beginPath(); g.moveTo(x(i0), mid - hv(i0));
      for (let i = i0 + 1; i <= i1; i++) g.lineTo(x(i), mid - hv(Math.min(i, i1 - 1)));
      for (let i = i1; i >= i0; i--) g.lineTo(x(i), mid + hv(Math.min(i, i1 - 1)));
      g.closePath(); g.fillStyle = fill; g.fill();
    }
    const TONE = {
      play: { on: mix(COL.play, .26, GLASS), ahead: mix(COL.play, .7, GLASS) },
      dub: { on: mix(COL.dub, .12, GLASS), ahead: mix(COL.dub, .62, GLASS), hot: mix(COL.white, .14, COL.dub) },
      muted: { on: [76, 81, 89], ahead: [44, 47, 52] },
    };
    return function (g, l, d, w, h, ph) {
      const n = Math.round(w * DPR), st = stateOf(l), T = TONE[st], mid = h / 2, amp = h / 2 - Math.min(AIR, h * .21); // the air scales with the lane, so a low lane keeps its reach
      const split = Math.max(0, Math.min(n, Math.round(ph * DPR)));
      const base = env(d, n), H = a => i => Math.max(.5, a[i] * amp);
      if (st === 'dub') {
        const all = env(SUM, n), nu = env(NEWS, n), hs = H(all);
        if (split > 0) {
          body(g, hs, 0, split, mid, css(T.on));
          body(g, i => Math.max(0, Math.min(nu[i] * amp * .66, hs(i) - 5)), 0, split, mid, css(T.hot));
        }
        if (split < n) body(g, H(base), split, n, mid, css(T.ahead));
        return;
      }
      if (split > 0) body(g, H(base), 0, split, mid, css(T.on));
      if (split < n) body(g, H(base), split, n, mid, css(T.ahead));
    };
  })();
  const paint = { a: paintA, b: paintB, c: paintC, f: paintF, g: paintG, h: paintH, j: paintJ, k: paintK, l: paintL, s: paintS, m: paintM }[(location.search.match(/[?&]w=([abcfghjklsm])/) || [])[1] || 'm'];
  /* WAVE END */

  const pane = document.getElementById('pane'), glass = pane.querySelector('.glass'), cv = document.getElementById('cv');
  const gw = glass.clientWidth, LH = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--lh')), gh = LH * 5, dpr = window.devicePixelRatio || 1;
  cv.style.width = gw + 'px'; cv.style.height = gh + 'px'; cv.width = Math.round(gw * dpr); cv.height = Math.round(gh * dpr);
  // the glass inner edge sits exactly on the lanes' top: the bezel reaches 4 px above and below (main.css)
  const g = cv.getContext('2d'); g.scale(dpr, dpr);
  const ph = Math.round(PH * gw);
  L.forEach((l, i) => {
    const y = i * LH, mid = LH / 2;
    g.save(); g.translate(0, y); g.beginPath(); g.rect(0, 0, gw, LH); g.clip();
    if (l.sel) { g.fillStyle = 'rgba(255,255,255,0.022)'; g.fillRect(0, 0, gw, LH); }
    // beat lines faint, bar lines a step up; the empty lane keeps only its bar lines, quieter
    for (let b = 1; b < 16; b++) {
      const x = Math.round(b / 16 * gw), bar = b % 4 === 0;
      if (l.empty && !bar) continue;
      g.fillStyle = bar ? `rgba(255,255,255,${l.empty ? .045 : .075})` : 'rgba(255,255,255,0.026)'; g.fillRect(x, 0, 1, LH);
    }
    if (l.empty) { for (let x = 6; x < gw; x += 10) { g.fillStyle = 'rgba(255,255,255,0.05)'; g.fillRect(x, Math.floor(mid), 4, 1); } }
    else { g.fillStyle = 'rgba(255,255,255,0.03)'; g.fillRect(0, Math.floor(mid), gw, 1); paint(g, l, W[i], gw, LH, ph); }
    g.restore();
    if (i) { g.fillStyle = 'rgba(0,0,0,0.95)'; g.fillRect(0, y, gw, 1); g.fillStyle = 'rgba(255,255,255,0.04)'; g.fillRect(0, y + 1, gw, 1); }
  });
  g.fillStyle = 'rgba(255,243,224,0.95)'; g.fillRect(ph, 0, 1, gh);
  g.beginPath(); g.moveTo(ph - 4, 0); g.lineTo(ph + 5, 0); g.lineTo(ph + .5, 6); g.closePath(); g.fill();
  for (let b = 0; b < 4; b++) glass.insertAdjacentHTML('beforeend', `<span class="bn" style="left:${Math.round(b / 4 * gw) + 6}px">${b + 1}</span>`);
})();
