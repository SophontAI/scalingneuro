const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

const nav = $("#nav");
const navLinks = $$('.nav-links a[href^="#"]');
const sections = navLinks
  .map((link) => ({ link, section: $(link.getAttribute("href")) }))
  .filter(({ section }) => section);

function updateNavigation() {
  nav?.classList.toggle("scrolled", window.scrollY > 20);
  const marker = window.scrollY + window.innerHeight * 0.35;
  let current = sections[0];
  for (const item of sections) {
    if (item.section.offsetTop <= marker) current = item;
  }
  for (const link of navLinks) {
    const active = link === current?.link;
    link.classList.toggle("is-current", active);
    if (active) link.setAttribute("aria-current", "location");
    else link.removeAttribute("aria-current");
  }
}

window.addEventListener("scroll", updateNavigation, { passive: true });
updateNavigation();

const terminalTabs = $$(".term-tab");
function activateTerminalTab(tab, moveFocus = false) {
  for (const item of terminalTabs) {
    const active = item === tab;
    item.classList.toggle("is-active", active);
    item.setAttribute("aria-selected", String(active));
    item.tabIndex = active ? 0 : -1;
  }
  for (const pane of $$(".term-pane")) {
    pane.classList.toggle("is-active", pane.dataset.pane === tab.dataset.tab);
  }
  if (moveFocus) tab.focus();
}

terminalTabs.forEach((tab, index) => {
  tab.addEventListener("click", () => activateTerminalTab(tab));
  tab.addEventListener("keydown", (event) => {
    let next = index;
    if (event.key === "ArrowRight") next = (index + 1) % terminalTabs.length;
    else if (event.key === "ArrowLeft") {
      next = (index - 1 + terminalTabs.length) % terminalTabs.length;
    } else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = terminalTabs.length - 1;
    else return;
    event.preventDefault();
    activateTerminalTab(terminalTabs[next], true);
  });
});

let toastTimer;
function showToast(message) {
  const element = $("#toast");
  if (!element) return;
  element.textContent = message;
  element.classList.add("show");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => element.classList.remove("show"), 2200);
}

const installCommands = {
  unix: "curl -fsSL https://scalingneuro.org/install.sh | sh",
  windows: "irm https://scalingneuro.org/install.ps1 | iex",
};
const installCommandElement = $("#installCommand");
const installPlatform = $("#installPlatform");
const platformToggle = $("#platformToggle");
let installPlatformName = /Windows/i.test(navigator.userAgent)
  ? "windows"
  : "unix";

function updateInstallPlatform() {
  const windows = installPlatformName === "windows";
  if (installCommandElement) {
    installCommandElement.textContent = installCommands[installPlatformName];
  }
  if (installPlatform) {
    installPlatform.textContent = windows
      ? "# Windows PowerShell"
      : "# macOS or Linux";
  }
  if (platformToggle) {
    platformToggle.textContent = windows ? "macOS / Linux" : "Microsoft Windows";
    platformToggle.setAttribute("aria-pressed", String(windows));
  }
}

updateInstallPlatform();

platformToggle?.addEventListener("click", () => {
  installPlatformName =
    installPlatformName === "windows" ? "unix" : "windows";
  updateInstallPlatform();
});

$("#copyBtn")?.addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(installCommands[installPlatformName]);
    showToast("Install command copied");
  } catch {
    showToast("Copy failed. Select the command above.");
  }
});

const accessForm = $("#accessForm");
const accessResult = $("#accessResult");
const formStatus = $("#formStatus");
const contributionChoices = accessForm
  ? $$('input[name="plans_to_contribute"]', accessForm)
  : [];
const accessAgreementCopy = accessForm
  ? $('[data-agreement-for="access"]', accessForm)
  : null;
const contributorAgreementCopy = accessForm
  ? $('[data-agreement-for="contributor"]', accessForm)
  : null;

function updateAccessAgreement() {
  const plansToContribute =
    contributionChoices.find((choice) => choice.checked)?.value === "yes";
  if (accessAgreementCopy) accessAgreementCopy.hidden = plansToContribute;
  if (contributorAgreementCopy) {
    contributorAgreementCopy.hidden = !plansToContribute;
  }
}

contributionChoices.forEach((choice) => {
  choice.addEventListener("change", updateAccessAgreement);
});
updateAccessAgreement();

accessForm?.addEventListener("submit", async (event) => {
  event.preventDefault();
  const submit = accessForm.querySelector('button[type="submit"]');
  const fields = new FormData(accessForm);
  const plansToContribute = fields.get("plans_to_contribute") === "yes";
  const dataUseAgreement = fields.get("data_use_agreement") === "on";
  const body = {
    contact_name: fields.get("contact_name"),
    contact_email: fields.get("contact_email"),
    institution_name: fields.get("institution_name"),
    lab_name: fields.get("lab_name"),
    plans_to_contribute: plansToContribute,
    contributor_attestation: plansToContribute && dataUseAgreement,
    accepted_contribution_policy_version: plansToContribute
      ? fields.get("accepted_contribution_policy_version")
      : null,
    data_use_agreement: dataUseAgreement,
    accepted_data_use_policy_version: fields.get(
      "accepted_data_use_policy_version",
    ),
  };

  submit.disabled = true;
  formStatus.textContent = "Submitting access request…";
  try {
    const response = await fetch("/v1/archive-access", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const payload = await response.json();
    if (!response.ok) {
      throw new Error(
        payload?.error?.message || "Access request could not be submitted",
      );
    }
    accessForm.reset();
    updateAccessAgreement();
    accessForm.hidden = true;
    accessResult.hidden = false;
    accessResult.scrollIntoView({ behavior: "smooth", block: "center" });
  } catch (error) {
    formStatus.textContent =
      error instanceof Error
        ? error.message
        : "Access request could not be submitted";
  } finally {
    submit.disabled = false;
  }
});
