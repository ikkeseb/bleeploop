/* MASKIN 2: the OTT face. Builds the face into every .ott on the page and keeps its glass alive.
   The glass is fed only what the engine has: three band levels and three band gains, simulated here from a
   believable guitar part through a three-band upward/downward compressor at DEPTH 50, TIME 50, UP 100, DOWN 100.
   Switches: ?knob=render (or <html data-knob="render">) for the rendered cap; ?live runs the glass at 60 fps,
   otherwise the page holds the composed moment (the review renders are stills). */
(function () {
  const Q = new URLSearchParams(location.search);
  const RENDER = (Q.get('knob') || document.documentElement.dataset.knob) === 'render';
  const ZOOM = +(Q.get('zoom') || document.documentElement.dataset.zoom || 1);
  const STILL = !Q.has('live');
  const BASE = document.currentScript.src.replace(/ott\.js(\?.*)?$/, '');
  const T_FREEZE = 15.025;              // the composed moment (seconds into the simulated jam)

  /* ---------- the four controls ---------- */
  const P = { depth: .5, time: .5, up: 1, down: 1 };

  /* ---------- layout, face pixels ---------- */
  const ROW = { ly: 482, vy: 504 };    // one legend line and one value line across the row
  const CTL = [
    { id: 'depth', lg: 'DEPTH', v: P.depth, x: 318, y: 386, s: 172, hero: true },
    { id: 'time', lg: 'TIME', v: P.time, x: 528, y: 424, s: 92 },
    { id: 'up', lg: 'UP', v: P.up, x: 690, y: 424, s: 92, tri: 'up' },
    { id: 'down', lg: 'DOWN', v: P.down, x: 812, y: 424, s: 92, tri: 'dn' }
  ];

  // knob: -135..+135 deg. The hero shows its value as a ring of LED dots; the small ones have printed ticks.
  function knob(c) {
    const s = c.s, a = -135 + 270 * c.v;
    let deco = '';
    if (c.hero) {
      const n = 31, r = s / 2 + 13, W = s + 40;
      let d = '';
      for (let i = 0; i < n; i++) {
        const t = (-135 + 270 * i / (n - 1)) * Math.PI / 180, on = i / (n - 1) <= c.v + 1e-6;
        d += `<i class="${on ? 'on' : ''}" style="left:${(W / 2 + r * Math.sin(t)).toFixed(2)}px;top:${(W / 2 - r * Math.cos(t)).toFixed(2)}px"></i>`;
      }
      deco = `<div class="ring" style="width:${W}px;height:${W}px">${d}</div>`;
    } else {
      const n = 11, tl = 6, gap = 4, pad = gap + tl + 4, W = s + pad * 2, cc = W / 2, r0 = s / 2 + gap, r1 = r0 + tl;
      let t = '';
      for (let i = 0; i < n; i++) {
        const q = (-135 + 270 * i / (n - 1)) * Math.PI / 180, major = i === 0 || i === n - 1 || i === 5;
        const r2 = r1 + (major ? 1.5 : 0);
        t += `<line x1="${(cc + r0 * Math.sin(q)).toFixed(2)}" y1="${(cc - r0 * Math.cos(q)).toFixed(2)}" x2="${(cc + r2 * Math.sin(q)).toFixed(2)}" y2="${(cc - r2 * Math.cos(q)).toFixed(2)}" stroke="${major ? '#8f96a0' : '#555c65'}" stroke-width="1.4" stroke-linecap="round"/>`;
      }
      deco = `<svg class="ticks" width="${W}" height="${W}">${t}</svg>`;
    }
    const kd = (360 * 1.35 / (Math.PI * s)).toFixed(3) + 'deg';
    const pw = c.hero ? '3px' : '2.5px';
    const body = RENDER
      ? `<div class="shadow"></div><img src="${BASE}parts/relay/out/knob.png" alt=""><div class="ptr"></div>`
      : `<div class="body"></div><div class="cap"></div><div class="ptr"></div>`;
    return `<div class="k ${RENDER ? 'kr' : 'kb'}" style="--a:${a}deg;--kd:${kd};--pw:${pw}">${deco}${body}</div>`;
  }

  function face(el) {
    let h = `<div class="bezel scr"><div class="glass"><canvas></canvas></div></div>`;
    h += `<span class="wm" style="left:18px;top:398px">OTT</span>`;
    h += `<span class="pwr on" style="left:22px;top:${ROW.vy - 3}px"></span>`;
    h += `<span class="maker" style="left:58px;top:${ROW.vy + 4}px">BLEEPLOOP</span>`;
    CTL.forEach(c => {
      const pct = Math.round(c.v * 100);
      h += `<div class="ctl ${c.hero ? 'hero' : ''}" style="--x:${c.x}px;--y:${c.y}px;--s:${c.s}px;--ly:${ROW.ly}px;--vy:${ROW.vy}px">
        ${knob(c)}
        <span class="lg lgd">${c.tri ? `<i class="tri ${c.tri}"></i>` : ''}${c.lg}</span>
        <span class="num val">${pct}<span class="u">%</span></span></div>`;
    });
    if (document.body.classList.contains('p-open'))
      h += `<button class="close" aria-label="Close"><svg width="12" height="12" viewBox="0 0 12 12" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M2 2l8 8M10 2L2 10"/></svg></button>`;
    el.innerHTML = h;
    return el.querySelector('.scr');
  }

  /* ---------- the simulated signal (SIM-BEGIN): three band levels (dBFS) from a guitar part at 120 BPM ---------- */
  // Each band's level is a set of notes: a smooth onset (a 12 ms raised cosine), a fast settle and a slow decay,
  // over a quiet floor. LOW: a bass note every two bars that rings out. MID: chords on the eighths, sustained.
  // HIGH: picked notes on the beat, a few on the off-beat, that die fast.
  const hash = n => { const x = Math.sin(n * 127.1 + 311.7) * 43758.5453; return x - Math.floor(x); };
  const BANDS = [
    { name: 'LOW', step: 2, pat: [1, 0], ex: [1.62], A: -17, acc: 0, fall: 20, ft: .3, slope: 3.2, floor: -41, seed: 1,
      tu: -30, td: -12, att: .06, rel: .32 },
    { name: 'MID', step: .25, pat: [1, 1, 0, 1, 1, 0, 1, 1], A: -13, acc: 1.5, fall: 4, ft: .06, slope: 6, floor: -40, seed: 2,
      tu: -32, td: -24, att: .045, rel: .17 },
    { name: 'HIGH', step: .25, pat: [1, 0, 0, 1, 0, 0, 1, 0], A: -20, acc: 1, fall: 22, ft: .085, slope: 5, floor: -42, seed: 3,
      tu: -32, td: -14, att: .03, rel: .07 }
  ];
  function level(b, t) {
    let L = b.floor + .8 * Math.sin(t * 1.3 + b.seed) + .5 * Math.sin(t * 3.1 + b.seed * 2);
    const n1 = Math.floor(t / b.step), on = [];
    for (let n = Math.max(0, n1 - 40); n <= n1; n++) if (b.pat[n % b.pat.length]) on.push([n * b.step + (hash(n + b.seed * 97) - .5) * .014, n]);
    if (b.ex) for (let k = Math.max(0, Math.floor(t / (b.step * b.pat.length)) - 3); k * b.step * b.pat.length <= t; k++)
      for (const o of b.ex) on.push([k * b.step * b.pat.length + o, 1000 + k, -11]);
    for (const [th, n, off = 0] of on) {
      if (t < th) continue;
      const d = t - th, A = b.A + off + (n % 8 === 0 ? b.acc : 0) + (hash(n * 3 + b.seed) - .5) * 3;
      const onset = d < .012 ? .5 - .5 * Math.cos(Math.PI * d / .012) : 1;
      L = Math.max(L, A - b.fall * (1 - Math.exp(-d / b.ft)) - b.slope * d - (1 - onset) * 40);
    }
    return L;
  }
  // a three-band OTT in dB: downward above td, upward below tu (capped at +12), both scaled by DEPTH (50 % = 1),
  // and the gain smoothed by two one-pole stages (attack when the gain falls, release, from TIME, when it rises),
  // so a compressor's gain reads as it is: rounded, never a tooth. Returns the gain history and the level now.
  const FS = 400, HIST = 3;
  function run(b, t) {
    const n = HIST * FS, warm = 2 * FS, out = new Float32Array(n), lv = new Float32Array(n);
    const rel = b.rel * (.5 + P.time), k = 2 * P.depth;
    let g1 = 0, g2 = 0, L = 0;
    const a = tau => 1 - Math.exp(-1 / (FS * tau));
    for (let i = -warm; i < n; i++) {
      const tt = t - HIST + (i + 1) / FS;
      L = level(b, tt);
      const down = L > b.td ? -(L - b.td) * .6 : 0;
      const up = L < b.tu ? Math.min(14, (b.tu - L) * .7) * Math.min(1, Math.max(0, (L + 66) / 8)) : 0;
      const target = Math.max(-12, Math.min(12, k * (P.up * up + P.down * down)));
      g1 += (target - g1) * a(target < g1 ? b.att : rel);
      g2 += (g1 - g2) * a(g1 < g2 ? b.att : rel);
      if (i >= 0) { out[i] = g2; lv[i] = L; }
    }
    return { hist: out, lv, g: g2, L };
  }
  /* SIM-END */

  /* ---------- the glass ---------- */
  // Per band, left to right: the last 3 s of its gain as a lit body between the 0 dB line and the gain (fine
  // columns, as the main screen's waveform, brightest at their edge, fading into the past); NOW as a wide LED
  // ladder of 1 dB cells centred on the 0 line, its newest edge running into it; the band's level as a neutral
  // meter at the band's right. 7 px per dB, +-12 dB.
  function glass(scr) {
    const cv = scr.querySelector('canvas'), box = scr.querySelector('.glass');
    const W = box.clientWidth, H = box.clientHeight, dpr = (window.devicePixelRatio || 1) * ZOOM;
    cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr);
    cv.style.width = W + 'px'; cv.style.height = H + 'px';
    const g = cv.getContext('2d');

    const X0 = 40, X1 = W - 6, BW = (X1 - X0) / 3, Y0 = 120, PX = 7;  // the 0 dB line; 7 px per dB
    const Y = db => Y0 - db * PX;
    const LAD = 58, MW = 6, PAD = 12, GAP = 10;                         // ladder, level meter, margins
    const bx = i => X0 + BW * i;
    const geo = i => {
      const r = Math.round(bx(i + 1)) - PAD, mx = r - MW, lx = mx - GAP - LAD;
      return { tx0: Math.round(bx(i)) + PAD, tx1: lx - 4, lx, mx };
    };

    // printed labels (HTML, so the type is the face's type)
    let lab = '';
    [12, 6, 0, -6, -12].forEach(d => lab += `<span class="t db num ${d ? '' : 'z'}" style="left:6px;top:${Y(d)}px">${d > 0 ? '+' + d : d < 0 ? '−' + (-d) : 0}</span>`);
    BANDS.forEach((b, i) => lab += `<span class="t lg band" style="left:${geo(i).tx0}px;top:12px">${b.name}</span>`);
    BANDS.forEach((b, i) => lab += `<span class="t gv num" style="left:${geo(i).lx + LAD}px;top:9px"></span>`);
    [['88', 'Hz'], ['2.5', 'kHz']].forEach(([v, u], i) => lab += `<span class="t hz num" style="left:${Math.round(bx(i + 1))}px;top:${H - 19}px">${v}<span class="u">${u}</span></span>`);
    box.insertAdjacentHTML('beforeend', lab);
    const gvs = [...box.querySelectorAll('.gv')];

    const LIFT = [255, 243, 224], CUT = [255, 176, 46], NEUT = [214, 219, 225];
    const rgba = (c, a) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
    const UNLIT = '#121417';

    function grid() {
      // +-6 and +-12 as fine lines, the engraved 0 dB line across the whole glass
      for (const d of [12, 6, -6, -12]) { g.fillStyle = Math.abs(d) === 12 ? 'rgba(255,255,255,.06)' : 'rgba(255,255,255,.04)'; g.fillRect(X0 - 4, Y(d), X1 - X0 + 4, 1); }
      g.fillStyle = '#000'; g.fillRect(X0 - 4, Y0 + 1, X1 - X0 + 4, 1);
      g.fillStyle = 'rgba(255,255,255,.2)'; g.fillRect(X0 - 4, Y0, X1 - X0 + 4, 1);
      // the band dividers, broken where the crossover is printed
      g.fillStyle = 'rgba(255,255,255,.09)';
      for (let i = 1; i < 3; i++) { const x = Math.round(bx(i)); g.fillRect(x, 8, 1, H - 32); }
      // one-second ticks under each band's history, on the -12 line
      g.fillStyle = 'rgba(255,255,255,.16)';
      BANDS.forEach((b, i) => { const { tx0, tx1 } = geo(i); for (let s = 0; s <= HIST; s++) g.fillRect(Math.round(tx1 - (tx1 - tx0) * s / HIST), Y(-12) + 1, 1, 4); });
    }

    function band(i, r) {
      const { tx0, tx1, lx, mx } = geo(i), w = tx1 - tx0, h = r.hist, n = h.length;
      // the range of the gain in each pixel column of the history
      const mn = new Float32Array(w), mxv = new Float32Array(w);
      for (let x = 0; x < w; x++) {
        const s0 = Math.floor(x / w * n), s1 = Math.max(s0 + 1, Math.floor((x + 1) / w * n));
        let lo = 1e9, hi = -1e9; for (let s = s0; s < s1; s++) { const v = h[Math.min(n - 1, s)]; if (v < lo) lo = v; if (v > hi) hi = v; }
        mn[x] = lo; mxv[x] = hi;
      }
      const fade = x => { const u = x / (w - 1); return Math.min(1, u * 6) * (.16 + .84 * Math.pow(u, 1.25)); };
      // the body: 2 px columns on a 3 px pitch, from the 0 line (dim) to the gain (bright)
      for (let x = 0; x + 2 <= w; x += 3) {
        const hi = Math.max(mxv[x], mxv[x + 1]), lo = Math.min(mn[x], mn[x + 1]), f = fade(x + 1);
        for (const [v, c, a1] of [[hi, LIFT, .85], [lo, CUT, .85]]) {
          if (c === LIFT ? v <= .05 : v >= -.05) continue;
          const ye = Y(v), gr = g.createLinearGradient(0, Y0, 0, ye);
          gr.addColorStop(0, rgba(c, .1 * f)); gr.addColorStop(.55, rgba(c, a1 * .5 * f)); gr.addColorStop(1, rgba(c, a1 * f));
          g.fillStyle = gr;
          if (c === LIFT) g.fillRect(tx0 + x, ye, 2, Y0 - ye); else g.fillRect(tx0 + x, Y0 + 1, 2, ye - Y0 - 1);
        }
      }
      // the edge: per pixel column, the range of the gain lit in full, at least 1.5 px, with a bloom of 2 px
      for (const pass of [0, 1]) {
        g.shadowBlur = pass ? 0 : 3;
        for (let x = 0; x < w; x++) {
          const f = fade(x), lo = mn[x], hi = mxv[x];
          for (const [c, a, b2] of [[LIFT, Math.max(0, lo), hi], [CUT, Math.min(0, hi), lo]]) {
            if (c === LIFT ? b2 <= .05 : b2 >= -.05) continue;
            let y0 = Math.min(Y(a), Y(b2)), y1 = Math.max(Y(a), Y(b2));
            if (y1 - y0 < 1.5) { const m = (c === LIFT ? Y(b2) + .75 : Y(b2) - .75); y0 = m - .75; y1 = m + .75; }
            g.shadowColor = rgba(c, .5 * f);
            g.fillStyle = rgba(c, (pass ? .95 : .35) * f); g.fillRect(tx0 + x, y0, 1, y1 - y0);
          }
        }
      }
      g.shadowBlur = 0;

      // NOW: 12 cells of 1 dB each way from the 0 line; the cell the value ends in lit by its fraction
      const gv = r.g, up = gv >= 0, col = up ? LIFT : CUT, mag = Math.abs(gv);
      for (let k = 1; k <= 12; k++) {
        const yu = Y0 - k * PX + 1, yd = Y0 + (k - 1) * PX + 2;
        g.fillStyle = UNLIT; g.beginPath(); g.roundRect(lx, yu, LAD, PX - 2, 1); g.roundRect(lx, yd, LAD, PX - 2, 1); g.fill();
        const f = Math.max(0, Math.min(1, mag - (k - 1)));
        if (f > .03) {
          g.shadowColor = rgba(col, .55 * f); g.shadowBlur = 4;
          g.fillStyle = rgba(col, .1 + .9 * f); g.beginPath(); g.roundRect(lx, up ? yu : yd, LAD, PX - 2, 1); g.fill();
          g.shadowBlur = 0;
        }
      }

      // the band's level: a neutral meter, -60..0 dBFS over the scale's height, 3 px cells on a 4 px pitch,
      // the top cell lit by its fraction, the last second's peak held
      const top = Y(12) + 1, bot = Y(-12), nc = Math.floor((bot - top) / 4), dbc = 60 / nc;
      const cells = v => (v + 60) / dbc;
      let pk = -99; for (let s = n - FS; s < n; s++) pk = Math.max(pk, r.lv[s]);
      const c0 = cells(r.L), cp = Math.floor(cells(pk));
      for (let k = 0; k < nc; k++) {
        const y = bot - (k + 1) * 4 + 1, f = Math.max(0, Math.min(1, c0 - k));
        g.fillStyle = UNLIT; g.fillRect(mx, y, MW, 3);
        if (f > .03) { g.shadowColor = rgba(NEUT, .25); g.shadowBlur = 2; g.fillStyle = rgba(NEUT, .1 + .56 * f); g.fillRect(mx, y, MW, 3); g.shadowBlur = 0; }
        else if (k === cp) { g.fillStyle = rgba(NEUT, .6); g.fillRect(mx, y, MW, 3); }
      }

      const v = Math.abs(gv) < .05 ? 0 : gv;
      gvs[i].className = `t gv num ${v > 0 ? 'up' : v < 0 ? 'dn' : ''}`;
      gvs[i].innerHTML = `${v > 0 ? '+' : v < 0 ? '−' : ''}${Math.abs(v).toFixed(1)}<span class="u">dB</span>`;
    }

    function draw(t) {
      g.setTransform(dpr, 0, 0, dpr, 0, 0);
      g.clearRect(0, 0, W, H);
      grid();
      BANDS.forEach((b, i) => band(i, run(b, t)));
    }

    if (STILL) draw(T_FREEZE);
    else { const t0 = performance.now() / 1000 - T_FREEZE; const loop = () => { draw(performance.now() / 1000 - t0); requestAnimationFrame(loop); }; loop(); }
    window.__ott = { draw, run, BANDS };
  }

  document.querySelectorAll('.ott').forEach(el => glass(face(el)));
})();
