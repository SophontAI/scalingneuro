/* ========================================================================== 
   NeuroScale — interactions + local NIfTI / synthetic 3D viewer
   ========================================================================== */

/* ---------- helpers ---------- */
const $  = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

/* ---------- fixed visual palette ---------- */
const NETWORK_A = '52,67,111';
const NETWORK_B = '173,95,102';

/* ---------- nav scroll state ---------- */
const nav = $('#nav');
const navLinks = $$('.nav-links a[href^="#"]');
const navSections = navLinks.map((link) => ({ link, section: $(link.getAttribute('href')) })).filter((item) => item.section);
const onScroll = () => {
  nav.classList.toggle('scrolled', window.scrollY > 24);
  const marker = window.scrollY + window.innerHeight * 0.34;
  let current = navSections[0];
  navSections.forEach((item) => { if (item.section.offsetTop <= marker) current = item; });
  navLinks.forEach((link) => {
    const active = link === current?.link;
    link.classList.toggle('is-current', active);
    if (active) link.setAttribute('aria-current', 'location');
    else link.removeAttribute('aria-current');
  });
};
window.addEventListener('scroll', onScroll, { passive: true });
onScroll();

/* ---------- reveal on scroll ---------- */
const io = new IntersectionObserver((entries) => {
  entries.forEach((e) => {
    if (e.isIntersecting) { e.target.classList.add('in-view'); io.unobserve(e.target); }
  });
}, { threshold: 0.16 });
$$('.reveal').forEach((el) => io.observe(el));

/* ---------- count-up stats ---------- */
const countUp = (el) => {
  const target = +el.dataset.count;
  const suffix = el.dataset.suffix || '';
  const dur = 1400; let start = null;
  const step = (t) => {
    if (start === null) start = t;
    const p = clamp((t - start) / dur, 0, 1);
    const eased = 1 - Math.pow(1 - p, 3);
    const val = Math.round(target * eased);
    el.textContent = val.toLocaleString() + suffix;
    if (p < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
};
const statObs = new IntersectionObserver((entries) => {
  entries.forEach((e) => { if (e.isIntersecting) { countUp(e.target); statObs.unobserve(e.target); } });
}, { threshold: 0.6 });
$$('[data-count]').forEach((el) => statObs.observe(el));

/* ---------- scalebar fills ---------- */
$$('#scaleBars li').forEach((li) => { li.querySelector('.sb-fill').style.setProperty('--fw', li.dataset.w + '%'); });

/* ---------- hero neural canvas ---------- */
(function neural() {
  const cv = $('#neuralCanvas');
  if (!cv || reducedMotion) return;
  const ctx = cv.getContext('2d');
  let w, h, nodes, raf;
  const DENSITY = 0.00009;

  const resize = () => {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    w = cv.clientWidth; h = cv.clientHeight;
    cv.width = w * dpr; cv.height = h * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const count = clamp(Math.round(w * h * DENSITY), 30, 120);
    nodes = Array.from({ length: count }, () => ({
      x: Math.random() * w, y: Math.random() * h,
      vx: (Math.random() - 0.5) * 0.25, vy: (Math.random() - 0.5) * 0.25,
      r: Math.random() * 1.6 + 0.6,
    }));
  };

  const draw = () => {
    ctx.clearRect(0, 0, w, h);
    for (const n of nodes) {
      n.x += n.vx; n.y += n.vy;
      if (n.x < 0 || n.x > w) n.vx *= -1;
      if (n.y < 0 || n.y > h) n.vy *= -1;
    }
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const a = nodes[i], b = nodes[j];
        const dx = a.x - b.x, dy = a.y - b.y;
        const d2 = dx * dx + dy * dy;
        if (d2 < 20000) {
          const alpha = (1 - d2 / 20000) * 0.5;
          const grad = ctx.createLinearGradient(a.x, a.y, b.x, b.y);
          grad.addColorStop(0, `rgba(${NETWORK_A},${alpha})`);
          grad.addColorStop(1, `rgba(${NETWORK_B},${alpha})`);
          ctx.strokeStyle = grad; ctx.lineWidth = 0.7;
          ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
        }
      }
    }
    for (const n of nodes) {
      ctx.beginPath(); ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
      ctx.fillStyle = `rgba(${NETWORK_A},0.72)`; ctx.fill();
    }
    raf = requestAnimationFrame(draw);
  };

  const ro = new ResizeObserver(() => { resize(); });
  ro.observe(cv);
  resize(); draw();
})();

/* ---------- terminal tabs ---------- */
const terminalTabs = $$('.term-tab');
function activateTerminalTab(tab, moveFocus = false) {
  terminalTabs.forEach((item) => {
    const active = item === tab;
    item.classList.toggle('is-active', active);
    item.setAttribute('aria-selected', String(active));
    item.tabIndex = active ? 0 : -1;
  });
  $$('.term-pane').forEach((pane) => pane.classList.toggle('is-active', pane.dataset.pane === tab.dataset.tab));
  if (moveFocus) tab.focus();
}

terminalTabs.forEach((tab, index) => {
  tab.addEventListener('click', () => activateTerminalTab(tab));
  tab.addEventListener('keydown', (event) => {
    let next = index;
    if (event.key === 'ArrowRight') next = (index + 1) % terminalTabs.length;
    else if (event.key === 'ArrowLeft') next = (index - 1 + terminalTabs.length) % terminalTabs.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = terminalTabs.length - 1;
    else return;
    event.preventDefault();
    activateTerminalTab(terminalTabs[next], true);
  });
});

/* ---------- toast ---------- */
let toastTimer;
const toast = (msg) => {
  const t = $('#toast'); t.textContent = msg; t.classList.add('show');
  clearTimeout(toastTimer); toastTimer = setTimeout(() => t.classList.remove('show'), 2200);
};

/* ---------- copy / download ---------- */
$('#copyBtn')?.addEventListener('click', async () => {
  const cmd = 'bash neuro-sync-preview.sh --deface auto ./new_session_dicoms';
  try { await navigator.clipboard.writeText(cmd); toast('Preview command copied'); }
  catch { toast('Copy failed — command shown in the terminal'); }
});
$('#dlBtn')?.addEventListener('click', () => toast('Safe preview script downloaded'));

/* ==========================================================================
   Scan browser + 3D viewer
   ========================================================================== */

const LOCAL_NIFTI_ENABLED = ['localhost', '127.0.0.1'].includes(window.location.hostname);

const SCANS = [
  { pid: 'sub-1001', site: 'Local example', scanner: 'real NIfTI-1', field: 'LOCAL', ses: 'ses-01',
    mod: 'anat', title: 'T1w structural', res: '1.0mm', tr: '—', te: '—', vols: '1 vol', size: '9.7 MB',
    realNifti: true, source: 'sub-1001_T1w.nii.gz' },
  { pid: 'sub-a3f9', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-01',
    mod: 'bold', title: 'task-rest BOLD', res: '2.0mm', tr: '1.5s', te: '30ms', vols: '480 vol', size: '1.2 GB', seed: 11 },
  { pid: 'sub-a3f9', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-01',
    mod: 'bold', title: 'task-movie BOLD', res: '1.6mm', tr: '1.0s', te: '28ms', vols: '610 vol', size: '2.1 GB', seed: 12, hires: true },
  { pid: 'sub-a3f9', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-01',
    mod: 'anat', title: 'T1w MPRAGE', res: '0.8mm', tr: '2.3s', te: '2.9ms', vols: '1 vol', size: '18 MB', seed: 13, safe: 'defaced' },
  { pid: 'sub-a3f9', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-02',
    mod: 'bold', title: 'task-rest BOLD', res: '2.0mm', tr: '1.5s', te: '30ms', vols: '480 vol', size: '1.2 GB', seed: 14 },

  { pid: 'sub-b7c2', site: 'McGill', scanner: 'Siemens Terra', field: '7T', ses: 'ses-01',
    mod: 'bold', title: 'movie-watching BOLD', res: '1.2mm', tr: '0.8s', te: '22ms', vols: '900 vol', size: '4.6 GB', seed: 21, hires: true },
  { pid: 'sub-b7c2', site: 'McGill', scanner: 'Siemens Terra', field: '7T', ses: 'ses-01',
    mod: 'dwi', title: 'diffusion dir98', res: '1.5mm', tr: '3.2s', te: '89ms', vols: '99 vol', size: '740 MB', seed: 22 },

  { pid: 'sub-c1e8', site: 'ENIGMA site 042', scanner: 'GE SIGNA', field: '3T', ses: 'ses-01',
    mod: 'bold', title: 'resting-state BOLD', res: '2.4mm', tr: '2.0s', te: '35ms', vols: '300 vol', size: '820 MB', seed: 31 },
  { pid: 'sub-c1e8', site: 'ENIGMA site 042', scanner: 'GE SIGNA', field: '3T', ses: 'ses-01',
    mod: 'anat', title: 'T1w BRAVO', res: '1.0mm', tr: '2.4s', te: '3.1ms', vols: '1 vol', size: '14 MB', seed: 32, safe: 'defaced' },

  { pid: 'sub-d5a1', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-01',
    mod: 'bold', title: 'naturalistic BOLD', res: '1.8mm', tr: '1.2s', te: '30ms', vols: '720 vol', size: '2.8 GB', seed: 41 },
  { pid: 'sub-d5a1', site: 'Princeton (PNI)', scanner: 'Siemens Prisma', field: '3T', ses: 'ses-01',
    mod: 'dwi', title: 'diffusion dir64', res: '2.0mm', tr: '4.1s', te: '95ms', vols: '65 vol', size: '410 MB', seed: 42 },
].filter((scan) => !scan.realNifti || LOCAL_NIFTI_ENABLED);

const MOD_META = {
  bold: { label: 'EPI', dir: 'func', chip: 'mchip-bold', seq: 'BOLD EPI' },
  dwi:  { label: 'DWI', dir: 'dwi',  chip: 'mchip-dwi', seq: 'DIFFUSION' },
  anat: { label: 'T1w', dir: 'anat', chip: 'mchip', seq: 'T1w MPRAGE' },
};
const fileName = (s) => {
  if (s.realNifti) return s.source;
  const folder = MOD_META[s.mod].dir;
  return `${folder}/${s.title.replace(/\s+/g, '-').toLowerCase()}.nii.gz`;
};

/* ---------- render archive (grouped by participant, file-tree style) ---------- */
const scanList = $('#scanList');
let activeFilter = 'all';

function renderList() {
  const groups = {};
  SCANS.forEach((s, i) => { (groups[s.pid] ??= []).push({ ...s, idx: i }); });
  scanList.innerHTML = '';
  let shown = 0;

  Object.entries(groups).forEach(([pid, scans]) => {
    const visible = scans.filter((s) => activeFilter === 'all' || s.mod === activeFilter);
    if (!visible.length) return;
    const meta = scans[0];
    const header = document.createElement('div');
    header.className = 'sc-group';
    header.textContent = `${pid}/  ·  ${meta.field}  ·  ${meta.scanner}`;
    scanList.appendChild(header);

    visible.forEach((s) => {
      shown++;
      const card = document.createElement('button');
      card.className = 'scan-card' + (s.realNifti ? ' sc-real' : '');
      card.dataset.idx = s.idx;
      const m = MOD_META[s.mod];
      const hires = s.hires ? '<span class="mchip mchip-hi">↑ hi-res</span>' : '';
      const safe = s.safe ? `<span class="mchip mchip-sage">${s.safe}</span>` : '';
      const policy = s.realNifti
        ? '<span class="mchip mchip-real">real NIfTI</span><span class="mchip sc-policy">local example</span>'
        : s.mod === 'anat'
        ? '<span class="mchip mchip-sage">privacy-cleared concept</span>'
        : '<span class="mchip mchip-sage">share path</span>';
      card.type = 'button';
      card.setAttribute('aria-label', `${s.pid}, ${s.ses}, ${s.title}, ${s.res}, ${s.size}`);
      card.setAttribute('aria-pressed', 'false');
      card.innerHTML = `
        <div class="sc-row"><span class="sc-name">${fileName(s)}</span><span class="sc-size">${s.size}</span></div>
        <div class="sc-sub"><span class="mchip ${m.chip}">${m.label}</span><span>${s.ses}</span><span>${s.res}</span><span>TR ${s.tr}</span>${hires}${safe}${policy}</div>`;
      card.addEventListener('click', () => selectScan(s, card, true));
      scanList.appendChild(card);
    });
  });

  if (!shown) scanList.innerHTML = '<p style="padding:22px;color:var(--ink-faint);font-family:var(--mono);font-size:12px">// no scans match this filter</p>';
}

$$('.filt').forEach((b) => b.addEventListener('click', () => {
  $$('.filt').forEach((x) => { x.classList.remove('is-active'); x.setAttribute('aria-pressed', 'false'); });
  b.classList.add('is-active'); activeFilter = b.dataset.filt; renderList();
  b.setAttribute('aria-pressed', 'true');
}));
$$('.filt').forEach((b) => b.setAttribute('aria-pressed', String(b.classList.contains('is-active'))));
renderList();

/* ---------- viewer elements ---------- */
const stage = $('#viewerStage');
const emptyEl = $('#viewerEmpty');
const loadingEl = $('#viewerLoading');
const loadLog = $('#loadLog');
const hudEl = $('#viewerHud');
const hudPath = $('#hudPath');
const controlsEl = $('#viewerControls');
const viewerTitle = $('#viewerTitle');
const viewerMeta = $('#viewerMeta');
const viewerMode = $('#viewerMode');
const archiveRoot = $('#archiveRoot');
const archiveState = $('#archiveState');

let three = null;          // { THREE, OrbitControls } once loaded
let scene = null, camera = null, renderer = null, controls = null;
let planes = {}, glass = null, activations = [], volume = null;
let N = 64, radii = { x: 1.0, y: 1.22, z: 0.95 };
let spinning = true, rafId = null, threeFailed = false;

/* lazy-load three.js so a CDN failure degrades gracefully */
async function ensureThree() {
  if (three) return true;
  if (threeFailed) return false;
  try {
    const THREE = await import('three');
    const { OrbitControls } = await import('three/addons/controls/OrbitControls.js');
    three = { THREE, OrbitControls };
    return true;
  } catch (e) {
    threeFailed = true;
    console.warn('three.js failed to load', e);
    return false;
  }
}

/* ---------- synthetic MRI volume ---------- */
function hash3(x, y, z, seed) {
  let n = (x * 374761393 + y * 668265263 + z * 1013904223 + seed * 6971) | 0;
  n = (n ^ (n >>> 13)) >>> 0;
  n = (n * 1274126177) >>> 0;
  return (n & 0xffffff) / 0x1000000;
}
function smooth(t) { return t * t * (3 - 2 * t); }
function valueNoise(x, y, z, seed) {
  const xi = Math.floor(x), yi = Math.floor(y), zi = Math.floor(z);
  const xf = x - xi, yf = y - yi, zf = z - zi;
  const u = smooth(xf), v = smooth(yf), w = smooth(zf);
  const c000 = hash3(xi, yi, zi, seed),     c100 = hash3(xi + 1, yi, zi, seed);
  const c010 = hash3(xi, yi + 1, zi, seed), c110 = hash3(xi + 1, yi + 1, zi, seed);
  const c001 = hash3(xi, yi, zi + 1, seed),     c101 = hash3(xi + 1, yi, zi + 1, seed);
  const c011 = hash3(xi, yi + 1, zi + 1, seed), c111 = hash3(xi + 1, yi + 1, zi + 1, seed);
  const x00 = c000 + (c100 - c000) * u, x10 = c010 + (c110 - c010) * u;
  const x01 = c001 + (c101 - c001) * u, x11 = c011 + (c111 - c011) * u;
  const y0 = x00 + (x10 - x00) * v, y1 = x01 + (x11 - x01) * v;
  return y0 + (y1 - y0) * w;
}
function fbm(x, y, z, seed, oct) {
  let amp = 0.5, freq = 1, sum = 0, norm = 0;
  for (let i = 0; i < oct; i++) { sum += amp * valueNoise(x * freq, y * freq, z * freq, seed + i * 37); norm += amp; amp *= 0.5; freq *= 2; }
  return sum / norm;
}

function buildVolume(scan) {
  const n = N;
  const vol = new Float32Array(n * n * n);
  const seed = scan.seed || 7;
  // per-modality look
  const isAnat = scan.mod === 'anat';
  const isDwi = scan.mod === 'dwi';
  const freq = isAnat ? 5.5 : (scan.hires ? 4.5 : 3.2);   // EPI = coarser/blurrier
  const contrast = isAnat ? 1.55 : (isDwi ? 1.2 : 1.35);
  const c = (n - 1) / 2;
  for (let z = 0; z < n; z++) {
    for (let y = 0; y < n; y++) {
      for (let x = 0; x < n; x++) {
        // normalized position within ellipsoid (-1..1)
        const nx = (x - c) / (c * radii.x * 0.98);
        const ny = (y - c) / (c * radii.y * 0.98);
        const nz = (z - c) / (c * radii.z * 0.98);
        const r = Math.sqrt(nx * nx / (radii.x * radii.x) + ny * ny / (radii.y * radii.y) + nz * nz / (radii.z * radii.z));
        let val = 0;
        if (r < 1.02) {
          const base = fbm(x / n * freq, y / n * freq, z / n * freq, seed, isAnat ? 4 : 3);
          const ribbon = smooth(clamp((r - 0.62) / 0.34, 0, 1));  // brighter cortical ribbon near surface
          const wm = 1 - ribbon;
          val = 0.34 * base + 0.42 * ribbon + 0.30 * wm * (0.5 + 0.5 * base);
          // ventricles: dark near center
          const cd = Math.sqrt(nx * nx + ny * ny * 0.6 + nz * nz);
          const vent = Math.exp(-(cd * cd) / 0.06);
          val *= (1 - 0.7 * vent);
          // sulci striations for anatomical crispness
          if (isAnat) val *= 0.75 + 0.25 * Math.sin(base * 22 + r * 8);
          // edge falloff
          val *= smooth(clamp((1.02 - r) / 0.12, 0, 1));
          val = clamp(Math.pow(val, contrast) * 1.12, 0, 1);
        }
        vol[x + y * n + z * n * n] = val;
      }
    }
  }
  return vol;
}

/* ---------- real local NIfTI-1 volume ---------- */
const NIFTI_TYPES = {
  2:    { name: 'uint8',   bytes: 1, read: (v, o) => v.getUint8(o) },
  4:    { name: 'int16',   bytes: 2, read: (v, o, le) => v.getInt16(o, le) },
  8:    { name: 'int32',   bytes: 4, read: (v, o, le) => v.getInt32(o, le) },
  16:   { name: 'float32', bytes: 4, read: (v, o, le) => v.getFloat32(o, le) },
  64:   { name: 'float64', bytes: 8, read: (v, o, le) => v.getFloat64(o, le) },
  256:  { name: 'int8',    bytes: 1, read: (v, o) => v.getInt8(o) },
  512:  { name: 'uint16',  bytes: 2, read: (v, o, le) => v.getUint16(o, le) },
  768:  { name: 'uint32',  bytes: 4, read: (v, o, le) => v.getUint32(o, le) },
};

async function fetchNiftiBuffer(url) {
  const response = await fetch(url, { cache: 'no-store' });
  if (!response.ok) throw new Error(`local file returned ${response.status}`);
  const source = await response.arrayBuffer();
  const bytes = new Uint8Array(source, 0, Math.min(2, source.byteLength));
  const gzipped = bytes[0] === 0x1f && bytes[1] === 0x8b;
  if (!gzipped) return source;
  if (typeof DecompressionStream === 'undefined') throw new Error('this browser cannot decompress .nii.gz files');
  const stream = new Blob([source]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Response(stream).arrayBuffer();
}

function percentile(sorted, q) {
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.floor(q * (sorted.length - 1))))];
}

function parseNiftiVolume(buffer, targetSize = 96) {
  if (buffer.byteLength < 352) throw new Error('file is too small to contain a NIfTI-1 volume');
  const view = new DataView(buffer);
  let littleEndian;
  if (view.getInt32(0, true) === 348) littleEndian = true;
  else if (view.getInt32(0, false) === 348) littleEndian = false;
  else throw new Error('NIfTI-1 header marker was not found');

  const ndim = view.getInt16(40, littleEndian);
  if (ndim < 3) throw new Error('viewer requires a 3D NIfTI volume');
  const dims = [42, 44, 46].map((offset) => view.getInt16(offset, littleEndian));
  if (dims.some((d) => !Number.isFinite(d) || d < 2)) throw new Error('invalid NIfTI dimensions');
  const voxelCount = dims[0] * dims[1] * dims[2];
  const datatypeCode = view.getInt16(70, littleEndian);
  const datatype = NIFTI_TYPES[datatypeCode];
  if (!datatype) throw new Error(`NIfTI datatype ${datatypeCode} is not supported by this prototype`);
  const voxelOffset = Math.floor(view.getFloat32(108, littleEndian));
  const neededBytes = voxelOffset + voxelCount * datatype.bytes;
  if (voxelOffset < 348 || neededBytes > buffer.byteLength) throw new Error('NIfTI voxel payload is incomplete');

  const voxelSize = [80, 84, 88].map((offset) => Math.abs(view.getFloat32(offset, littleEndian)) || 1);
  let slope = view.getFloat32(112, littleEndian);
  let intercept = view.getFloat32(116, littleEndian);
  if (!Number.isFinite(slope) || slope === 0) slope = 1;
  if (!Number.isFinite(intercept)) intercept = 0;
  const readScaled = (index) => datatype.read(view, voxelOffset + index * datatype.bytes, littleEndian) * slope + intercept;

  // Derive robust display bounds from positive, finite voxels. This keeps empty
  // background transparent while retaining cortical intensity differences.
  const sampled = [];
  const sampleStep = Math.max(1, Math.ceil(voxelCount / 180000));
  for (let i = 0; i < voxelCount; i += sampleStep) {
    const value = readScaled(i);
    if (Number.isFinite(value) && value > 0.01) sampled.push(value);
  }
  if (sampled.length < 100) throw new Error('NIfTI volume does not contain enough finite image data');
  sampled.sort((a, b) => a - b);
  const low = percentile(sampled, 0.01);
  const high = percentile(sampled, 0.995);
  if (!(high > low)) throw new Error('NIfTI intensity range is empty');

  const n = targetSize;
  const output = new Float32Array(n * n * n);
  const [dx, dy, dz] = dims;
  const plane = dx * dy;
  for (let z = 0; z < n; z++) {
    const sourceZ = Math.round((z / (n - 1)) * (dz - 1));
    for (let y = 0; y < n; y++) {
      const sourceY = Math.round((y / (n - 1)) * (dy - 1));
      for (let x = 0; x < n; x++) {
        const sourceX = Math.round((x / (n - 1)) * (dx - 1));
        const sourceIndex = sourceX + sourceY * dx + sourceZ * plane;
        const scaled = (readScaled(sourceIndex) - low) / (high - low);
        const normalized = scaled <= 0.01 ? 0 : Math.pow(clamp(scaled, 0, 1), 0.82);
        output[x + y * n + z * n * n] = normalized;
      }
    }
  }

  const physical = dims.map((d, i) => d * voxelSize[i]);
  const maxPhysical = Math.max(...physical);
  output._meta = {
    real: true,
    dims,
    voxelSize,
    datatype: datatype.name,
    intensity: [low, high],
    physical,
    radii: { x: physical[0] / maxPhysical, y: physical[1] / maxPhysical, z: physical[2] / maxPhysical },
  };
  return output;
}

async function loadNiftiVolume(scan) {
  const buffer = await fetchNiftiBuffer(scan.source);
  return parseNiftiVolume(buffer, 96);
}

function setVolumeGeometry(meta) {
  radii = meta?.real ? { ...meta.radii } : { x: 1.0, y: 1.22, z: 0.95 };
  if (glass?._grp) glass._grp.scale.set(radii.x, radii.z, radii.y);
  if (glass) glass.visible = !meta?.real;
  if (glass?._wire) glass._wire.visible = !meta?.real;
  if (glass) glass.material.opacity = meta?.real ? 0 : 0.05;
  if (glass?._wire) glass._wire.material.opacity = meta?.real ? 0 : 0.09;
}

/* build an RGBA DataTexture for one slice */
function sliceTexture(axis, index) {
  const { THREE } = three;
  const n = N;
  const data = new Uint8Array(n * n * 4);
  for (let a = 0; a < n; a++) {
    for (let b = 0; b < n; b++) {
      let v;
      if (axis === 'z')      v = volume[a + b * n + index * n * n];         // axial: a=x, b=y
      else if (axis === 'y') v = volume[a + index * n + b * n * n];         // coronal: a=x, b=z
      else                   v = volume[index + a * n + b * n * n];         // sagittal: a=y, b=z
      const g = Math.round(clamp(v, 0, 1) * 255);
      const i = (a + b * n) * 4;
      // Warm tissue is tuned by CSS for the dark viewer surface.
      data[i]     = g;
      data[i + 1] = Math.round(g * 0.965);
      data[i + 2] = Math.round(g * 0.83);
      data[i + 3] = g < 8 ? 0 : 255;   // transparent background outside brain
    }
  }
  const tex = new THREE.DataTexture(data, n, n, THREE.RGBAFormat);
  tex.minFilter = tex.magFilter = THREE.LinearFilter;
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.needsUpdate = true;
  return tex;
}

function initThreeScene() {
  const { THREE, OrbitControls } = three;
  const canvas = $('#brainCanvas');
  renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  scene = new THREE.Scene();
  camera = new THREE.PerspectiveCamera(42, 16 / 11, 0.1, 100);
  camera.position.set(2.4, 1.5, 2.9);

  scene.add(new THREE.AmbientLight(0xffffff, 0.9));
  const l1 = new THREE.PointLight(0xffe5c6, 2.0, 20); l1.position.set(3, 3, 4); scene.add(l1);
  const l2 = new THREE.PointLight(0x9aa7d1, 1.6, 20); l2.position.set(-3, -1, -3); scene.add(l2);

  // glass brain shell
  const geo = new THREE.SphereGeometry(1, 48, 32);
  const mat = new THREE.MeshStandardMaterial({ color: 0x8f9bc2, transparent: true, opacity: 0.07, roughness: 0.5, metalness: 0.1, side: THREE.FrontSide, depthWrite: false });
  glass = new THREE.Mesh(geo, mat);
  const wire = new THREE.Mesh(geo, new THREE.MeshBasicMaterial({ color: 0xb3bad2, wireframe: true, transparent: true, opacity: 0.12 }));
  const grp = new THREE.Group(); grp.add(glass); grp.add(wire); scene.add(grp);
  grp.scale.set(radii.x, radii.z, radii.y);   // three Y is up → map anatomical z to world Y
  glass._grp = grp;
  glass._wire = wire;

  controls = new OrbitControls(camera, canvas);
  controls.enableDamping = true; controls.dampingFactor = 0.08;
  controls.minDistance = 2.0; controls.maxDistance = 7;
  controls.autoRotate = spinning; controls.autoRotateSpeed = 1.1;
  controls.enablePan = false;

  window.addEventListener('resize', resizeRenderer);
  const ro = new ResizeObserver(resizeRenderer); ro.observe(stage);
}

function resizeRenderer() {
  if (!renderer) return;
  const w = stage.clientWidth, h = stage.clientHeight;
  renderer.setSize(w, h, false);
  camera.aspect = w / h; camera.updateProjectionMatrix();
}

/* create/update the three slice planes for the current volume */
function buildPlanes() {
  const { THREE } = three;
  // remove old
  Object.values(planes).forEach((p) => { scene.remove(p); p.geometry.dispose(); p.material.map?.dispose(); p.material.dispose(); });
  planes = {};
  const mk = (axis, w, hgt) => {
    const g = new THREE.PlaneGeometry(w, hgt);
    const m = new THREE.MeshBasicMaterial({ transparent: true, side: THREE.DoubleSide, map: sliceTexture(axis, Math.floor(N / 2)), depthWrite: false });
    return new THREE.Mesh(g, m);
  };
  // world half-extents (three Y up = anatomical z)
  const ex = radii.x, ey = radii.y, ez = radii.z;
  // axial (normal = superior/inferior = three Y)
  const ax = mk('z', 2 * ex, 2 * ey); ax.rotation.x = -Math.PI / 2;
  // coronal (normal = anterior/posterior = three Z)
  const co = mk('y', 2 * ex, 2 * ez);
  // sagittal (normal = left/right = three X)
  const sa = mk('x', 2 * ey, 2 * ez); sa.rotation.y = Math.PI / 2;
  planes = { z: ax, y: co, x: sa };
  Object.values(planes).forEach((p) => scene.add(p));
  updatePlane('z', 50); updatePlane('y', 50); updatePlane('x', 50);
}

function updatePlane(axis, pct) {
  if (!planes[axis]) return;
  const { THREE } = three;
  const idx = Math.round(clamp(pct / 100, 0, 1) * (N - 1));
  const p = planes[axis];
  p.material.map?.dispose();
  p.material.map = sliceTexture(axis, idx);
  p.material.needsUpdate = true;
  const frac = (idx / (N - 1) - 0.5) * 2;   // -1..1
  if (axis === 'z') p.position.y = frac * radii.z;   // axial moves along three Y
  if (axis === 'y') p.position.z = frac * radii.y;   // coronal along three Z
  if (axis === 'x') p.position.x = frac * radii.x;   // sagittal along three X
}

function buildActivations(scan) {
  const { THREE } = three;
  activations.forEach((group) => {
    scene.remove(group);
    group.children.forEach((mesh) => {
      mesh.geometry?.dispose();
      mesh.material?.dispose();
    });
  });
  activations = [];
  if (scan.mod !== 'bold') return;   // activation hotspots only for functional
  const colors = [0xe3a0a5, 0xaab7df, 0xe1b475, 0x9bc7b5];
  const count = 4;
  for (let i = 0; i < count; i++) {
    const rnd = (k) => hash3(i * 7 + k, scan.seed, 3, scan.seed) - 0.5;
    const core = 0.05 + Math.abs(rnd(9)) * 0.045;
    const grp = new THREE.Group();
    // bright core + soft additive halo → reads as a glowing activation focus
    const coreMesh = new THREE.Mesh(
      new THREE.SphereGeometry(core, 16, 12),
      new THREE.MeshBasicMaterial({ color: colors[i % colors.length], transparent: true, opacity: 0.92, depthWrite: false }));
    const halo = new THREE.Mesh(
      new THREE.SphereGeometry(core * 2.6, 16, 12),
      new THREE.MeshBasicMaterial({ color: colors[i % colors.length], transparent: true, opacity: 0.16, blending: THREE.AdditiveBlending, depthWrite: false }));
    grp.add(coreMesh); grp.add(halo);
    grp.position.set(rnd(1) * 1.1 * radii.x, rnd(2) * 1.0 * radii.z, rnd(3) * 1.1 * radii.y);
    grp._phase = i * 1.3;
    scene.add(grp); activations.push(grp);
  }
}

function animate() {
  rafId = requestAnimationFrame(animate);
  const t = performance.now() / 1000;
  controls.autoRotate = spinning;
  controls.update();
  activations.forEach((b) => {
    const pulse = 0.5 + 0.5 * Math.sin(t * 2 + b._phase);
    b.scale.setScalar(0.85 + pulse * 0.35);
    if (b.children[1]) b.children[1].material.opacity = 0.12 + pulse * 0.22;
  });
  renderer.render(scene, camera);
}

/* ---------- select + "stream" a scan ---------- */
let loadToken = 0;
async function selectScan(scan, card, userInitiated = false) {
  $$('.scan-card').forEach((c) => { c.classList.remove('is-active'); c.setAttribute('aria-pressed', 'false'); });
  card?.classList.add('is-active');
  card?.setAttribute('aria-pressed', 'true');

  const token = ++loadToken;
  emptyEl.hidden = true;
  hudEl.hidden = true;
  controlsEl.hidden = true;
  viewerMeta.hidden = true;
  loadingEl.hidden = false;
  loadLog.textContent = '';

  const path = scan.realNifti
    ? `local://${scan.source}`
    : `s3://scaling-neuro-concept/${scan.pid}/${scan.ses}/${fileName(scan)}`;
  const steps = scan.realNifti ? [
    `↪ reading local example ${scan.source} …`,
    `decompressing ${scan.size} NIfTI-1 volume in this browser`,
  ] : scan.mod === 'anat' ? [
    `↪ simulating pull of a privacy-cleared structural scan …`,
    `local face processing + privacy QC passed`,
    `rendering synthetic ${scan.res} structural preview`,
    `no live scan is read or transferred`,
  ] : [
    `↪ simulating pull from s3://scaling-neuro-concept …`,
    `GET ${scan.pid}/${scan.ses}/  (${scan.field} · ${scan.scanner})`,
    `synthetic payload ${scan.size} · ${scan.vols} · ${scan.res}`,
    `reconstructing ${N}³ concept grid …`,
  ];
  const ok = await ensureThree();
  if (token !== loadToken) return;

  if (!ok) { showThreeFallback(scan, path); return; }

  // play the streaming log
  for (let i = 0; i < steps.length; i++) {
    if (token !== loadToken) return;
    loadLog.textContent += (i ? '\n' : '') + steps[i];
    await new Promise((r) => setTimeout(r, reducedMotion ? 40 : 320));
  }

  // build scene lazily once
  if (!scene) initThreeScene();

  try {
    if (scan.realNifti) {
      N = 96;
      volume = await loadNiftiVolume(scan);
      if (token !== loadToken) return;
      const meta = volume._meta;
      loadLog.textContent += `\nheader ${meta.dims.join('×')} · ${meta.voxelSize.map((v) => `${v.toFixed(1)}mm`).join(' × ')}`;
      loadLog.textContent += `\nnormalizing real voxels → ${N}³ interactive grid`;
      setVolumeGeometry(meta);
    } else {
      N = 64;
      setVolumeGeometry(null);
      volume = buildVolume(scan);
    }
  } catch (error) {
    if (token !== loadToken) return;
    showVolumeError(scan, path, error);
    return;
  }
  volume._mod = scan.mod;
  buildPlanes();
  buildActivations(scan);
  resizeRenderer();
  // reset sliders
  $$('#viewerControls input[type=range]').forEach((s) => { s.value = 50; });
  if (!rafId) animate();

  if (token !== loadToken) return;
  loadingEl.hidden = true;
  hudEl.hidden = false; controlsEl.hidden = false;
  hudPath.textContent = path;
  viewerTitle.textContent = `${scan.pid} · ${scan.title}`;
  const m = MOD_META[scan.mod];
  viewerMode.textContent = scan.realNifti ? 'Instant View · real local NIfTI' : 'Instant View · synthetic';
  stage.setAttribute('aria-label', `Interactive 3D preview of ${scan.pid}, ${scan.title}`);
  archiveRoot.innerHTML = scan.realNifti ? 'local://examples/ <b>real file</b>' : 's3://scaling-neuro/ <b>concept</b>';
  archiveState.textContent = scan.realNifti ? 'same-origin' : 'planned';
  viewerMeta.hidden = false;
  if (scan.realNifti) {
    const meta = volume._meta;
    viewerMeta.innerHTML =
      `DIM: ${meta.dims.join(' × ')}<br/>` +
      `RES: ${meta.voxelSize.map((v) => `${v.toFixed(1)}mm`).join(' × ')}<br/>` +
      `TYPE: NIfTI-1 · ${meta.datatype}<br/>` +
      `STATE: REAL_LOCAL_FILE · NOT_SHARED`;
  } else {
    viewerMeta.innerHTML =
      `RES: ${scan.res} ISO<br/>` +
      `SEQ: ${m.seq}<br/>` +
      `FIELD: ${scan.field} · TR ${scan.tr}<br/>` +
      `STATE: ${scan.mod === 'anat' ? 'LOCAL_ONLY_CONCEPT' : 'SYNTHETIC_PULL'}${scan.safe ? ' · DEFACED' : ''}`;
  }
  if (userInitiated && window.matchMedia('(max-width: 760px)').matches) {
    stage.closest('.vault-right')?.scrollIntoView({ behavior: reducedMotion ? 'auto' : 'smooth', block: 'start' });
  }
}

function showThreeFallback(scan, path) {
  loadingEl.hidden = true;
  emptyEl.hidden = false;
  emptyEl.innerHTML = `<svg viewBox="0 0 24 24" width="40" height="40"><use href="#i-cube"/></svg>
    <p><strong>${scan.pid} · ${scan.title}</strong><br/>The 3D layer needs WebGL and its rendering library.
    The selected data path is <code style="font-size:11px">${path}</code>.</p>`;
}

function showVolumeError(scan, path, error) {
  console.error('volume decode failed', error);
  loadingEl.hidden = true;
  hudEl.hidden = true;
  controlsEl.hidden = true;
  viewerMeta.hidden = true;
  emptyEl.hidden = false;
  emptyEl.innerHTML = `<p><strong>${scan.pid} · ${scan.title}</strong><br/>The real local NIfTI could not be decoded.<br/>
    <code>${String(error.message || error)}</code><br/><small>${path}</small></p>`;
}

/* slider + spin controls */
$$('#viewerControls input[type=range]').forEach((sl) => {
  let queued = false;
  sl.addEventListener('input', () => {
    if (queued) return; queued = true;
    requestAnimationFrame(() => { updatePlane(sl.dataset.axis, +sl.value); queued = false; });
  });
});
$('#spinBtn')?.addEventListener('click', () => {
  spinning = !spinning;
  const button = $('#spinBtn');
  button.classList.toggle('is-active', spinning);
  button.setAttribute('aria-pressed', String(spinning));
  button.setAttribute('aria-label', spinning ? 'Pause auto-rotate' : 'Resume auto-rotate');
  button.title = spinning ? 'Pause auto-rotate' : 'Resume auto-rotate';
});

/* Load the real local T1w example so the viewer proves an actual NIfTI path on arrival. */
requestAnimationFrame(() => {
  const firstCard = $('.scan-card[data-idx="0"]');
  if (firstCard) selectScan(SCANS[0], firstCard);
});
