(() => {
  "use strict";

  const action = document.getElementById("install-action");
  const panel = document.getElementById("install-progress");
  if (!action || !panel || !window.fetch || !window.AbortController) return;

  const heading = document.getElementById("progress-heading");
  const detail = document.getElementById("progress-detail");
  const bar = document.getElementById("progress-bar");
  const amount = document.getElementById("progress-amount");
  const ios = panel.dataset.platform === "ios";
  const originalHref = action.href;
  const originalLabel = action.textContent;
  const originalAria = action.getAttribute("aria-label");
  const pollInterval = 1000;
  const waitingTimeout = 15000;
  const stallTimeout = 20000;
  let started = false;
  let stopped = false;
  let timer;
  let request;
  let clickedAt = 0;
  let lastChangeAt = 0;
  let lastStreamedBytes = 0;
  let lastPhase = "waiting";
  let failures = 0;

  function message(title, description) {
    // Avoid repeatedly announcing unchanged text to screen readers.
    if (heading.textContent !== title) heading.textContent = title;
    if (detail.textContent !== description) detail.textContent = description;
  }

  function button(label, disabled = false) {
    action.textContent = label;
    action.setAttribute("aria-label", label);
    action.setAttribute("aria-disabled", String(disabled));
  }

  function stop(title, description) {
    stopped = true;
    clearTimeout(timer);
    message(title, description);
    action.href = window.location.pathname;
    button("Reload install page");
  }

  function bytes(value) {
    if (value < 1024) return `${value} B`;
    const unit = value < 1024 * 1024 ? "KB" : value < 1024 ** 3 ? "MB" : "GB";
    const divisor = unit === "KB" ? 1024 : unit === "MB" ? 1024 ** 2 : 1024 ** 3;
    return `${(value / divisor).toFixed(1)} ${unit}`;
  }

  function render(state) {
    const now = Date.now();
    const total = state.total_bytes;
    const sent = state.bytes_sent;
    const streamed = state.bytes_streamed;
    if (!Number.isSafeInteger(total) || total < 0 || !Number.isSafeInteger(sent) || sent < 0 ||
        !Number.isSafeInteger(streamed) || streamed < sent ||
        sent > total || !["waiting", "preparing", "transferring", "interrupted", "transferred"].includes(state.phase) ||
        !["installable", "expired", "limit_reached"].includes(state.availability)) {
      throw new Error("Invalid transfer status");
    }
    // Retransmitting an already-covered prefix is activity even though the
    // unique-byte percentage cannot advance until the retry catches up.
    if (streamed !== lastStreamedBytes || state.phase !== lastPhase) lastChangeAt = now;
    lastStreamedBytes = streamed;
    lastPhase = state.phase;
    const hasProgress = sent > 0 || state.phase === "transferring" || state.phase === "transferred";
    bar.hidden = amount.hidden = !hasProgress;
    if (hasProgress) {
      // A rounded number must not imply completion before full transfer coverage.
      const percent = total > 0 ? Math.floor((sent / total) * 100) : 0;
      bar.value = percent;
      amount.textContent = `${percent}% · ${bytes(sent)} / ${bytes(total)}`;
    }
    if (state.phase === "transferred") {
      stop("Package transfer complete", ios
        ? "Check your Home Screen. iOS still needs to verify and install the app."
        : "Open the APK from your browser’s downloads to install it.");
      button("Start again");
      return;
    }
    // An expiry or exhausted quota need not cancel a response already streaming.
    if (state.availability !== "installable" && state.phase !== "transferring") {
      stop(state.availability === "expired" ? "Link expired" : "Download limit reached",
        ios ? "Check your Home Screen. If the app is missing, ask for a new share link."
          : "Check your downloads. If the APK is missing, ask for a new share link.");
      return;
    }
    if (state.phase === "transferring") {
      const stalled = now - lastChangeAt >= stallTimeout;
      message(stalled ? "Waiting for transfer to continue" : "Transferring package…", stalled
        ? "No new data yet. Check your connection and keep the sharing computer running. You can try again if needed."
        : "Keep the sharing computer running while the package transfers.");
      button(stalled ? "Try again" : "Transferring…", !stalled);
    } else if (state.phase === "interrupted") {
      message("Transfer interrupted", "Waiting for your device to resume. Check your connection, or tap Try again.");
      button("Try again");
    } else if (now - clickedAt >= waitingTimeout) {
      message("Download has not started", ios
        ? "Confirm Install in the system prompt. If no prompt appeared, open this page in Safari and try again. You can also check your Home Screen."
        : "Check your browser’s downloads. If nothing appeared, try again.");
      button("Try again");
    } else {
      message(state.phase === "preparing" ? "Preparing download…" : ios ? "Confirm on your device" : "Waiting for download…", ios
        ? "Tap Install in the system prompt. The package transfer will appear here when it starts."
        : "Your browser will download the APK. Transfer progress will appear here when it starts.");
      button(ios ? "Waiting for confirmation…" : "Starting download…", true);
    }
  }

  function schedule(delay = pollInterval) {
    clearTimeout(timer);
    if (started && !stopped && !document.hidden) timer = setTimeout(poll, delay);
  }

  async function poll() {
    if (!started || stopped || document.hidden || request) return;
    const controller = new AbortController();
    request = controller;
    const timeout = setTimeout(() => controller.abort(), 8000);
    try {
      const response = await fetch(panel.dataset.statusUrl, {
        cache: "no-store", credentials: "same-origin", signal: controller.signal,
      });
      if ([403, 404, 410].includes(response.status)) {
        stop("Progress session ended", ios
          ? "Check your Home Screen. Reload this page if you need to start another installation."
          : "Check your downloads. Reload this page if you need to download again.");
        return;
      }
      if (!response.ok) throw new Error("Status unavailable");
      const state = await response.json();
      if (document.hidden) return;
      render(state);
      failures = 0;
    } catch (_error) {
      if (document.hidden) return;
      failures += 1;
      // Loss of the share server says nothing about whether iOS installed the app.
      message("Progress unavailable", ios
        ? "The share connection may have closed. Check your Home Screen; installation may still be continuing. Reconnecting…"
        : "The share connection may have closed. Check your browser’s downloads. Reconnecting…");
      button("Try again");
      if (failures >= 12) {
        stop("Progress unavailable", ios
          ? "Check your Home Screen for the app. If it is missing, reconnect or ask for a new share link."
          : "Check your downloads. If the APK is missing, reconnect or ask for a new share link.");
      }
    } finally {
      clearTimeout(timeout);
      request = undefined;
      schedule(failures ? 5000 : pollInterval);
    }
  }

  action.addEventListener("click", (event) => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || stopped) return;
    if (action.getAttribute("aria-disabled") === "true") {
      event.preventDefault();
      return;
    }
    started = true;
    clickedAt = lastChangeAt = Date.now();
    failures = 0;
    panel.hidden = false;
    action.href = originalHref;
    message(ios ? "Confirm on your device" : "Starting download…", ios
      ? "Tap Install in the system prompt. You can return here to check the package transfer."
      : "Your browser will download the APK. Open it after the download finishes.");
    button(ios ? "Waiting for confirmation…" : "Starting download…", true);
    // On short screens the button starts at the viewport's bottom. Bring the
    // newly revealed feedback into view without delaying the native hand-off.
    panel.scrollIntoView({ block: "nearest" });
    // Keep native link navigation in the original user gesture: no awaited fetch,
    // hidden iframe, or JavaScript re-download of a potentially multi-GB package.
    schedule(0);
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      clearTimeout(timer);
      if (request) request.abort();
    } else {
      schedule(0);
    }
  });
  window.addEventListener("pagehide", () => {
    clearTimeout(timer);
    if (request) request.abort();
  });
  window.addEventListener("pageshow", () => {
    if (started) schedule(0);
    else {
      action.textContent = originalLabel;
      if (originalAria) action.setAttribute("aria-label", originalAria);
    }
  });
})();
