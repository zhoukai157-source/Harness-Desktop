// DSH Desktop — controller page.
//
// This is the page bundled into the app. It boots the engine and lets Rust
// navigate the window to the real DeepSeek Harness UI. While the window lives
// on the engine origin, desktop controls live in the tray/menu (Rust side) —
// this page is only shown during startup, engine install, and on errors.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const els = {
  statusText: document.querySelector("#status-text"),
  status: document.querySelector("#status"),
  progress: document.querySelector("#progress"),
  progressBar: document.querySelector("#progress-bar"),
  progressText: document.querySelector("#progress-text"),
  urlRow: document.querySelector("#url-row"),
  url: document.querySelector("#url"),
  openBrowser: document.querySelector("#open-browser"),
  errorBox: document.querySelector("#error-box"),
  errorText: document.querySelector("#error-text"),
  retry: document.querySelector("#retry"),
  setWorkspace: document.querySelector("#set-workspace"),
};

let raf = 0;

// Smoothly animate the progress bar to `target` percent. The phases only
// convey coarse milestones, so the motion in between is cosmetic — exactly
// the "even a fake progress bar is fine" kind of feedback.
function animateProgress(target, text) {
  if (text != null) els.progressText.textContent = text;
  els.progress.hidden = false;
  const bar = els.progressBar;
  const cur = parseFloat(bar.style.width) || 0;
  if (raf) cancelAnimationFrame(raf);
  const start = performance.now();
  const dur = 450;
  const ease = (t) => 1 - Math.pow(1 - t, 3);
  const step = (now) => {
    const t = Math.min(1, (now - start) / dur);
    bar.style.width = `${Math.round((cur + (target - cur) * ease(t)) * 10) / 10}%`;
    if (t < 1) raf = requestAnimationFrame(step);
    else raf = 0;
  };
  raf = requestAnimationFrame(step);
}

function setState(state, text) {
  els.status.dataset.state = state;
  els.statusText.textContent = text;
  els.urlRow.hidden = true;
  if (state === "error") {
    els.errorBox.hidden = false;
    animateProgress(100, "Startup failed");
  } else if (state === "ready" || state === "attached") {
    animateProgress(100, text || "Engine ready");
  } else if (state === "starting") {
    animateProgress(85, text || "Starting the engine…");
  } else if (state === "installing") {
    animateProgress(40, text || "Installing the DSH engine…");
  } else if (state === "stopped") {
    els.progress.hidden = true;
  }
}

async function boot() {
  setState("starting", "Starting the engine…");
  els.errorBox.hidden = true;
  try {
    const info = await invoke("start_engine");
    // Rust navigates the window away on success; show the URL just in case.
    if (info && info.url) {
      showUrl(info.url);
      animateProgress(100, "Engine ready");
    }
  } catch (err) {
    setState("error", "Engine failed to start");
    els.errorText.textContent = String(err);
  }
}

function showUrl(url) {
  els.urlRow.hidden = false;
  els.url.textContent = url;
}

// Live progress from the Rust side (events also fire when the window is back
// here after a tray "Restart Engine" — this page is only shown in those cases).
listen("engine-status", (event) => {
  const { status, detail, url } = event.payload || {};
  if (status === "error") {
    setState("error", "Engine stopped unexpectedly");
    els.errorText.textContent = detail || "unknown error";
  } else if (status === "starting") {
    setState("starting", detail || "Starting the engine…");
  } else if (status === "installing") {
    setState("installing", detail || "Installing the DSH engine…");
  } else if (status === "ready" || status === "attached") {
    setState("ok", detail || "Engine ready");
    if (url) showUrl(url);
  } else if (status === "stopped") {
    setState("stopped", detail || "Engine stopped");
  }
});

els.openBrowser.addEventListener("click", async () => {
  try {
    await invoke("open_in_browser");
  } catch (err) {
    els.errorText.textContent = String(err);
    els.errorBox.hidden = false;
  }
});

els.retry.addEventListener("click", boot);
els.setWorkspace.addEventListener("click", () => invoke("set_workspace"));

window.addEventListener("DOMContentLoaded", boot);
