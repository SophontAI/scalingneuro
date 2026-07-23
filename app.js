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
    link.classList.toggle("is-current", link === current?.link);
  }
}

window.addEventListener("scroll", updateNavigation, { passive: true });
updateNavigation();

let toastTimer;
function showToast(message) {
  const element = $("#toast");
  if (!element) return;
  element.textContent = message;
  element.classList.add("show");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => element.classList.remove("show"), 2200);
}

const installCommand = /Windows/i.test(navigator.userAgent)
  ? "irm https://scalingneuro.com/install.ps1 | iex"
  : "curl -fsSL https://scalingneuro.com/install.sh | sh";
const installCommandElement = $("#installCommand");
const installPlatform = $("#installPlatform");
if (installCommandElement) installCommandElement.textContent = installCommand;
if (installPlatform && /Windows/i.test(navigator.userAgent)) {
  installPlatform.textContent = "# Windows PowerShell";
}

$("#copyBtn")?.addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(installCommand);
    showToast("Install command copied");
  } catch {
    showToast("Copy failed. Select the command above.");
  }
});

const accessForm = $("#accessForm");
const accessResult = $("#accessResult");
const formStatus = $("#formStatus");
let issuedAccessToken = "";

accessForm?.addEventListener("submit", async (event) => {
  event.preventDefault();
  const submit = accessForm.querySelector('button[type="submit"]');
  const fields = new FormData(accessForm);
  const body = {
    contact_name: fields.get("contact_name"),
    contact_email: fields.get("contact_email"),
    institution_name: fields.get("institution_name"),
    lab_name: fields.get("lab_name"),
    participation_commitment: fields.get("participation_commitment") === "on",
  };

  submit.disabled = true;
  formStatus.textContent = "Creating archive access…";
  try {
    const response = await fetch("/v1/archive-access", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const payload = await response.json();
    if (!response.ok) {
      throw new Error(payload?.error?.message || "Access could not be created");
    }
    issuedAccessToken = payload.access_token;
    $("#accessToken").textContent = issuedAccessToken;
    accessForm.hidden = true;
    accessResult.hidden = false;
    accessResult.scrollIntoView({ behavior: "smooth", block: "center" });
  } catch (error) {
    formStatus.textContent =
      error instanceof Error ? error.message : "Access could not be created";
  } finally {
    submit.disabled = false;
  }
});

$("#copyToken")?.addEventListener("click", async () => {
  if (!issuedAccessToken) return;
  try {
    await navigator.clipboard.writeText(issuedAccessToken);
    showToast("Archive token copied");
  } catch {
    showToast("Copy failed. Select the token manually.");
  }
});
