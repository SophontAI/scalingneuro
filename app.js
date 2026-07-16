/* ========================================================================== 
   NeuroScale — interactions + live OpenNeuro 3D viewer
   ========================================================================== */

import { fetchFirstNiftiVolume } from './nifti-preview.mjs?v=1';
import {
  datasetMatches,
  fetchOpenNeuroDataset,
  fetchPopularOpenNeuroDatasets,
} from './openneuro-client.mjs?v=1';

/* ---------- helpers ---------- */
const $  = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const escapeHtml = (value) => String(value ?? '').replace(/[&<>'"]/g, (character) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&quot;',
})[character]);
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
  const cmd = /Windows/i.test(navigator.userAgent)
    ? 'irm https://scalingneuro.com/install.ps1 | iex'
    : 'curl -fsSL https://scalingneuro.com/install.sh | sh';
  try { await navigator.clipboard.writeText(cmd); toast('Install command copied'); }
  catch { toast('Copy failed — command shown in the terminal'); }
});
$('#dlBtn')?.addEventListener('click', () => toast('Opening installer'));

/* ==========================================================================
   Scan browser + 3D viewer
   ========================================================================== */

let SCANS = [];
let popularDatasets = [];
let currentDataset = null;
let selectedScanId = null;
let datasetLoadController;

const MOD_META = {
  bold: { label: 'BOLD', chip: 'mchip-bold' },
  anat: { label: 'ANAT', chip: 'mchip-sage' },
  dwi: { label: 'DWI', chip: 'mchip-hi' },
  fmap: { label: 'FMAP', chip: 'mchip-hi' },
  other: { label: 'MRI', chip: 'mchip-sage' },
};
const fileName = (scan) => scan.filename || scan.path;

/* ---------- render live OpenNeuro archive ---------- */
const scanList = $('#scanList');
const fileCount = $('#fileCount');
const fileSearch = $('#fileSearch');
const datasetSearch = $('#datasetSearch');
const datasetResults = $('#datasetResults');
const datasetSelection = $('#datasetSelection');
const openneuroDatasetLink = $('#openneuroDatasetLink');
let activeFilter = 'bold';
let fileQuery = '';
let visibleFileLimit = 60;

function renderList() {
  const query = fileQuery.trim().toLowerCase();
  const matching = SCANS.filter((scan) => (
    (activeFilter === 'all' || scan.mod === activeFilter || (activeFilter === 'other' && ['fmap', 'other'].includes(scan.mod))) &&
    (!query || [scan.path, scan.pid, scan.ses, scan.task, scan.suffix].filter(Boolean).join(' ').toLowerCase().includes(query))
  ));
  scanList.innerHTML = '';
  fileCount.textContent = `${matching.length.toLocaleString()} previewable file${matching.length === 1 ? '' : 's'}`;

  matching.slice(0, visibleFileLimit).forEach((scan) => {
    const card = document.createElement('button');
    const meta = MOD_META[scan.mod] || MOD_META.other;
    card.className = `scan-card sc-real${scan.id === selectedScanId ? ' is-active' : ''}`;
    card.dataset.scanId = scan.id;
    card.type = 'button';
    card.setAttribute('aria-label', `Preview ${scan.path}, ${scan.size}`);
    card.setAttribute('aria-pressed', String(scan.id === selectedScanId));
    card.innerHTML = `
      <div class="sc-row"><span class="sc-name">${escapeHtml(fileName(scan))}</span><span class="sc-size">${escapeHtml(scan.size)}</span></div>
      <div class="sc-sub"><span class="mchip ${meta.chip}">${meta.label}</span><span>${escapeHtml(scan.pid)}</span><span>${escapeHtml(scan.ses)}</span>${scan.task ? `<span>${escapeHtml(scan.task)}</span>` : ''}</div>`;
    card.addEventListener('click', () => selectScan(scan, card, true));
    scanList.appendChild(card);
  });

  if (!matching.length) {
    scanList.innerHTML = '<div class="explorer-status"><strong>No matching files</strong>Try another modality or search term.</div>';
  } else if (matching.length > visibleFileLimit) {
    const showMore = document.createElement('button');
    showMore.className = 'show-more-files';
    showMore.type = 'button';
    showMore.textContent = `Show ${Math.min(60, matching.length - visibleFileLimit)} more`;
    showMore.addEventListener('click', () => { visibleFileLimit += 60; renderList(); });
    scanList.appendChild(showMore);
  }
}

$$('.filt').forEach((button) => button.addEventListener('click', () => {
  $$('.filt').forEach((item) => { item.classList.remove('is-active'); item.setAttribute('aria-pressed', 'false'); });
  button.classList.add('is-active');
  button.setAttribute('aria-pressed', 'true');
  activeFilter = button.dataset.filt;
  visibleFileLimit = 60;
  renderList();
}));
$$('.filt').forEach((button) => button.setAttribute('aria-pressed', String(button.classList.contains('is-active'))));
fileSearch?.addEventListener('input', () => {
  fileQuery = fileSearch.value;
  visibleFileLimit = 60;
  renderList();
});

function closeDatasetResults() {
  datasetResults.hidden = true;
  datasetSearch.setAttribute('aria-expanded', 'false');
}

function renderDatasetResults() {
  const query = datasetSearch.value.trim();
  const exactAccession = /^ds\d{6}$/i.test(query) ? query.toLowerCase() : null;
  const matches = popularDatasets.filter((dataset) => datasetMatches(dataset, query)).slice(0, 8);
  if (exactAccession && !matches.some((dataset) => dataset.id === exactAccession)) {
    matches.unshift({ id: exactAccession, name: 'Load this OpenNeuro accession', tasks: [], size: '' });
  }
  datasetResults.innerHTML = '';
  if (!matches.length) {
    datasetResults.innerHTML = '<div class="explorer-status"><strong>No featured match</strong>Paste an accession such as ds000001.</div>';
  } else {
    matches.forEach((dataset) => {
      const button = document.createElement('button');
      button.className = 'dataset-result';
      button.type = 'button';
      button.setAttribute('role', 'option');
      const context = [dataset.id, dataset.tasks?.slice(0, 2).join(' · '), dataset.size].filter(Boolean).join(' · ');
      button.innerHTML = `<strong>${escapeHtml(dataset.name)}</strong><span>${escapeHtml(context)}</span>`;
      button.addEventListener('click', () => loadDataset(dataset.id, true));
      datasetResults.appendChild(button);
    });
  }
  datasetResults.hidden = false;
  datasetSearch.setAttribute('aria-expanded', 'true');
}

datasetSearch?.addEventListener('focus', renderDatasetResults);
datasetSearch?.addEventListener('input', renderDatasetResults);
datasetSearch?.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closeDatasetResults();
  if (event.key === 'Enter') {
    event.preventDefault();
    const accession = datasetSearch.value.match(/ds\d{6}/i)?.[0]?.toLowerCase();
    const firstMatch = popularDatasets.find((dataset) => datasetMatches(dataset, datasetSearch.value));
    if (accession || firstMatch) loadDataset(accession || firstMatch.id, true);
  }
});
document.addEventListener('pointerdown', (event) => {
  if (!event.target.closest('.dataset-tools')) closeDatasetResults();
});
$$('.dataset-shortcuts button').forEach((button) => button.addEventListener('click', () => loadDataset(button.dataset.dataset, true)));

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
let planes = {}, glass = null, volume = null;
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

/* ---------- streamed OpenNeuro NIfTI-1 volume ---------- */
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

async function loadNiftiVolume(scan, signal) {
  const buffer = await fetchFirstNiftiVolume(scan.source, { signal });
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

function animate() {
  rafId = requestAnimationFrame(animate);
  controls.autoRotate = spinning;
  controls.update();
  renderer.render(scene, camera);
}

/* ---------- select + stream an OpenNeuro scan ---------- */
let loadToken = 0;
let volumeLoadController;
async function selectScan(scan, card, userInitiated = false) {
  selectedScanId = scan.id;
  $$('.scan-card').forEach((c) => { c.classList.remove('is-active'); c.setAttribute('aria-pressed', 'false'); });
  card?.classList.add('is-active');
  card?.setAttribute('aria-pressed', 'true');

  volumeLoadController?.abort();
  const controller = new AbortController();
  volumeLoadController = controller;
  const token = ++loadToken;
  emptyEl.hidden = true;
  hudEl.hidden = true;
  controlsEl.hidden = true;
  viewerMeta.hidden = true;
  loadingEl.hidden = false;
  loadLog.textContent = '';

  const path = `openneuro://${scan.datasetId}@${scan.snapshot}/${scan.path}`;
  const steps = [
    `↪ OpenNeuro ${scan.datasetId} snapshot ${scan.snapshot}`,
    `streaming only the first 3D volume from the ${scan.size} version-pinned object`,
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
    N = 96;
    volume = await loadNiftiVolume(scan, controller.signal);
    if (token !== loadToken) return;
    const meta = volume._meta;
    loadLog.textContent += `\nheader ${meta.dims.join('×')} · ${meta.voxelSize.map((v) => `${v.toFixed(1)}mm`).join(' × ')}`;
    loadLog.textContent += `\nnormalizing real voxels → ${N}³ interactive grid`;
    setVolumeGeometry(meta);
  } catch (error) {
    if (error?.name === 'AbortError') return;
    if (token !== loadToken) return;
    showVolumeError(scan, path, error);
    return;
  }
  volume._mod = scan.mod;
  buildPlanes();
  resizeRenderer();
  // reset sliders
  $$('#viewerControls input[type=range]').forEach((s) => { s.value = 50; });
  if (!rafId) animate();

  if (token !== loadToken) return;
  loadingEl.hidden = true;
  hudEl.hidden = false; controlsEl.hidden = false;
  hudPath.textContent = path;
  viewerTitle.textContent = fileName(scan);
  viewerMode.textContent = 'OpenNeuro · first-volume preview';
  stage.setAttribute('aria-label', `Interactive 3D preview of ${scan.path} from OpenNeuro ${scan.datasetId}`);
  archiveRoot.innerHTML = 'OpenNeuro <b>live</b>';
  archiveState.textContent = 'public';
  openneuroDatasetLink.href = scan.openNeuroUrl;
  openneuroDatasetLink.hidden = false;
  viewerMeta.hidden = false;
  const meta = volume._meta;
  viewerMeta.innerHTML =
    `DATASET: ${escapeHtml(scan.datasetId)} · ${escapeHtml(scan.snapshot)}<br/>` +
    `DIM: ${meta.dims.join(' × ')}<br/>` +
    `RES: ${meta.voxelSize.map((v) => `${v.toFixed(1)}mm`).join(' × ')}<br/>` +
    `TYPE: NIfTI-1 · ${escapeHtml(meta.datatype)}<br/>` +
    `STATE: VERSION_PINNED_S3 · FIRST_VOLUME`;
  if (userInitiated && window.matchMedia('(max-width: 760px)').matches) {
    stage.closest('.vault-right')?.scrollIntoView({ behavior: reducedMotion ? 'auto' : 'smooth', block: 'start' });
  }
}

function showThreeFallback(scan, path) {
  loadingEl.hidden = true;
  emptyEl.hidden = false;
  emptyEl.innerHTML = `<svg viewBox="0 0 24 24" width="40" height="40"><use href="#i-cube"/></svg>
    <p><strong>${escapeHtml(scan.datasetId)} · ${escapeHtml(fileName(scan))}</strong><br/>The 3D layer needs WebGL and its rendering library.
    The selected data path is <code style="font-size:11px">${escapeHtml(path)}</code>.</p>`;
}

function showVolumeError(scan, path, error) {
  console.error('volume decode failed', error);
  loadingEl.hidden = true;
  hudEl.hidden = true;
  controlsEl.hidden = true;
  viewerMeta.hidden = true;
  emptyEl.hidden = false;
  emptyEl.innerHTML = `<p><strong>${escapeHtml(scan.datasetId)} · ${escapeHtml(fileName(scan))}</strong><br/>This OpenNeuro NIfTI could not be streamed and decoded.<br/>
    <code>${escapeHtml(error.message || error)}</code><br/><small>${escapeHtml(path)}</small></p>`;
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

async function loadDataset(id, userInitiated = false) {
  datasetLoadController?.abort();
  volumeLoadController?.abort();
  const controller = new AbortController();
  datasetLoadController = controller;
  const accession = String(id || '').trim().toLowerCase();

  closeDatasetResults();
  selectedScanId = null;
  currentDataset = null;
  SCANS = [];
  fileSearch.value = '';
  fileQuery = '';
  datasetSearch.value = accession;
  fileCount.textContent = 'Loading previewable files…';
  scanList.innerHTML = `<div class="explorer-status is-loading"><strong>Opening ${escapeHtml(accession)}</strong>Reading the latest public snapshot from OpenNeuro…</div>`;
  datasetSelection.innerHTML = `<strong>${escapeHtml(accession)}</strong><span>OpenNeuro public dataset</span>`;
  $$('.dataset-shortcuts button').forEach((button) => button.classList.toggle('is-active', button.dataset.dataset === accession));
  openneuroDatasetLink.hidden = true;
  loadingEl.hidden = true;
  hudEl.hidden = true;
  controlsEl.hidden = true;
  viewerMeta.hidden = true;
  emptyEl.hidden = false;
  emptyEl.innerHTML = '<svg viewBox="0 0 24 24" width="40" height="40"><use href="#i-cube"/></svg><p>Loading preview files from OpenNeuro…</p>';

  try {
    const dataset = await fetchOpenNeuroDataset(accession, { signal: controller.signal });
    if (controller.signal.aborted) return;
    currentDataset = dataset;
    SCANS = dataset.scans;
    activeFilter = SCANS.some((scan) => scan.mod === 'bold') ? 'bold' : 'all';
    visibleFileLimit = 60;
    $$('.filt').forEach((button) => {
      const active = button.dataset.filt === activeFilter;
      button.classList.toggle('is-active', active);
      button.setAttribute('aria-pressed', String(active));
    });
    datasetSearch.value = dataset.id;
    datasetSelection.innerHTML = `<strong>${escapeHtml(dataset.id)} · ${escapeHtml(dataset.name)}</strong><span>snapshot ${escapeHtml(dataset.snapshot)} · ${SCANS.length.toLocaleString()} previewable NIfTI files · ${escapeHtml(dataset.size)}${dataset.license ? ` · ${escapeHtml(dataset.license)}` : ''}</span>`;
    openneuroDatasetLink.href = `https://openneuro.org/datasets/${dataset.id}/versions/${dataset.snapshot}`;
    openneuroDatasetLink.hidden = false;
    renderList();

    const firstScan = SCANS.find((scan) => activeFilter === 'all' || scan.mod === activeFilter) || SCANS[0];
    const firstCard = $('.scan-card');
    if (firstScan && firstCard) await selectScan(firstScan, firstCard, userInitiated);
  } catch (error) {
    if (error?.name === 'AbortError' || controller.signal.aborted) return;
    console.error('OpenNeuro dataset load failed', error);
    fileCount.textContent = 'Unavailable';
    scanList.innerHTML = `<div class="explorer-status is-error"><strong>Could not open ${escapeHtml(accession)}</strong>${escapeHtml(error.message || error)}</div>`;
    datasetSelection.innerHTML = `<strong>${escapeHtml(accession)}</strong><span>Dataset unavailable</span>`;
    emptyEl.hidden = false;
    emptyEl.innerHTML = `<p><strong>OpenNeuro dataset unavailable</strong><br/>${escapeHtml(error.message || error)}</p>`;
  }
}

fetchPopularOpenNeuroDatasets({ first: 40 })
  .then((datasets) => {
    popularDatasets = datasets;
    if (document.activeElement === datasetSearch) renderDatasetResults();
  })
  .catch((error) => console.warn('Popular OpenNeuro datasets unavailable', error));

loadDataset('ds000001');
