/* ==========================================================================
   Scaling Neuro: site interactions
   ========================================================================== */

const $  = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];

/* ---------- nav scroll state ---------- */
const nav = $('#nav');
const navLinks = $$('.nav-links a[href^="#"]');
const navSections = navLinks
  .map((link) => ({ link, section: $(link.getAttribute('href')) }))
  .filter((item) => item.section);
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
  entries.forEach((entry) => {
    if (entry.isIntersecting) { entry.target.classList.add('in-view'); io.unobserve(entry.target); }
  });
}, { threshold: 0.16 });
$$('.reveal').forEach((el) => io.observe(el));

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
  const t = $('#toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('show'), 2200);
};

/* ---------- copy install command ---------- */
$('#copyBtn')?.addEventListener('click', async () => {
  const cmd = /Windows/i.test(navigator.userAgent)
    ? 'irm https://scalingneuro.com/install.ps1 | iex'
    : 'curl -fsSL https://scalingneuro.com/install.sh | sh';
  try { await navigator.clipboard.writeText(cmd); toast('Install command copied'); }
  catch { toast('Copy failed; use the command shown above'); }
});

/* ---------- count-up figures ---------- */
const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
if (!reducedMotion) {
  const fmt = new Intl.NumberFormat('en-US');
  const countUp = (el) => {
    const target = +el.dataset.count;
    const prefix = el.dataset.prefix || '';
    const suffix = el.dataset.suffix || '';
    const dur = 1500; let start = null;
    const step = (t) => {
      if (start === null) start = t;
      const p = Math.min((t - start) / dur, 1);
      const eased = 1 - Math.pow(1 - p, 3);
      el.textContent = prefix + fmt.format(Math.round(target * eased)) + suffix;
      if (p < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  };
  const countObs = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (entry.isIntersecting) { countUp(entry.target); countObs.unobserve(entry.target); }
    });
  }, { threshold: 0.6 });
  $$('[data-count]').forEach((el) => countObs.observe(el));
}
