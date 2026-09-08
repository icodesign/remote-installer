import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const scriptPath = fileURLToPath(new URL("../assets/install-progress.js", import.meta.url));
const script = readFileSync(scriptPath, "utf8");

class FakeElement {
  constructor(id) {
    this.id = id;
    this.dataset = {};
    this.hidden = true;
    this.textContent = "";
    this.value = 0;
    this.max = 100;
    this._href = "https://example.test/download";
    this.attributes = new Map();
    this.listeners = new Map();
    this.scrollCalls = [];
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  dispatch(type, event = {}) {
    const dispatched = {
      button: 0,
      defaultPrevented: false,
      preventDefault() {
        this.defaultPrevented = true;
      },
      ...event,
    };
    for (const listener of this.listeners.get(type) ?? []) listener(dispatched);
    return dispatched;
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null;
  }

  get href() {
    return this._href;
  }

  set href(value) {
    this._href = new URL(value, "https://example.test/install/app-1").href;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  scrollIntoView(options) {
    this.scrollCalls.push(options);
  }
}

function jsonResponse(body, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async json() {
      return body;
    },
  };
}

function createPage({ platform = "android" } = {}) {
  const elements = new Map(
    [
      ["install-action", new FakeElement("install-action")],
      ["install-progress", new FakeElement("install-progress")],
      ["progress-heading", new FakeElement("progress-heading")],
      ["progress-detail", new FakeElement("progress-detail")],
      ["progress-bar", new FakeElement("progress-bar")],
      ["progress-amount", new FakeElement("progress-amount")],
    ],
  );
  const action = elements.get("install-action");
  const panel = elements.get("install-progress");
  panel.dataset.statusUrl = "/status/grant-1";
  panel.dataset.platform = platform;
  action.textContent = platform === "ios" ? "Install" : "Download APK";
  action.setAttribute("aria-label", "Download the app");

  let now = 0;
  let nextTimer = 1;
  const timers = new Map();
  const fetchQueue = [];
  const fetchCalls = [];
  const documentListeners = new Map();
  const windowListeners = new Map();
  const document = {
    hidden: false,
    getElementById(id) {
      return elements.get(id) ?? null;
    },
    addEventListener(type, listener) {
      const listeners = documentListeners.get(type) ?? [];
      listeners.push(listener);
      documentListeners.set(type, listeners);
    },
    dispatch(type) {
      for (const listener of documentListeners.get(type) ?? []) listener();
    },
  };
  const window = {
    location: { pathname: "/install/app-1" },
    addEventListener(type, listener) {
      const listeners = windowListeners.get(type) ?? [];
      listeners.push(listener);
      windowListeners.set(type, listeners);
    },
  };

  function setFetchResponse(response) {
    fetchQueue.push(() => Promise.resolve(response));
  }

  function setFetchError(error = new Error("network down")) {
    fetchQueue.push(() => Promise.reject(error));
  }

  function setPendingFetch() {
    fetchQueue.push(({ signal }) => new Promise((_resolve, reject) => {
      signal?.addEventListener("abort", () => reject(new Error("request aborted")), { once: true });
    }));
  }

  function fetch(url, options = {}) {
    fetchCalls.push({ url, options });
    const next = fetchQueue.shift();
    return next ? next(options) : Promise.reject(new Error("unexpected fetch"));
  }

  function setTimeoutFake(callback, delay = 0) {
    const id = nextTimer++;
    timers.set(id, { callback, due: now + delay });
    return id;
  }

  function clearTimeoutFake(id) {
    timers.delete(id);
  }

  async function settle() {
    for (let index = 0; index < 8; index += 1) await Promise.resolve();
  }

  async function runNextTimer({ at } = {}) {
    const entries = [...timers.entries()].sort((left, right) => left[1].due - right[1].due);
    assert.ok(entries.length > 0, "expected a scheduled timer");
    const [id, timer] = entries[0];
    now = at === undefined ? Math.max(now, timer.due) : at;
    timers.delete(id);
    timer.callback();
    await settle();
  }

  const context = {
    AbortController,
    Date: { now: () => now },
    Promise,
    clearTimeout: clearTimeoutFake,
    document,
    fetch,
    setTimeout: setTimeoutFake,
    window,
  };
  window.fetch = fetch;
  window.AbortController = AbortController;
  vm.runInNewContext(script, context, { filename: scriptPath });

  return {
    action,
    elements,
    document,
    fetchCalls,
    runNextTimer,
    setFetchError,
    setFetchResponse,
    setPendingFetch,
    settle,
    timers,
    set now(value) {
      now = value;
    },
  };
}

function installableState(phase, bytesSent, totalBytes = 1000) {
  return {
    availability: "installable",
    bytes_sent: bytesSent,
    total_bytes: totalBytes,
    phase,
  };
}

function click(page, event = {}) {
  return page.action.dispatch("click", event);
}

async function startAndPoll(page) {
  const event = click(page);
  await page.runNextTimer();
  return event;
}

function isDisabled(action) {
  return action.getAttribute("aria-disabled") === "true";
}

test("a normal click gives immediate feedback while preserving native navigation and suppressing duplicates", async () => {
  const page = createPage();
  page.setFetchResponse(jsonResponse(installableState("waiting", 0)));

  const first = click(page);
  assert.equal(first.defaultPrevented, false);
  const panel = page.elements.get("install-progress");
  assert.equal(panel.hidden, false);
  assert.equal(panel.scrollCalls.length, 1);
  assert.equal(panel.scrollCalls[0].block, "nearest");
  assert.equal(isDisabled(page.action), true);
  assert.equal(page.fetchCalls.length, 0, "the status check must not replace the native user gesture");

  const duplicate = click(page);
  assert.equal(duplicate.defaultPrevented, true, "a duplicate click must not start another attempt");
  assert.equal(page.fetchCalls.length, 0);
  await page.runNextTimer();
  assert.equal(page.fetchCalls.length, 1);
});

test("modified and non-primary clicks remain untouched", () => {
  for (const event of [
    { metaKey: true },
    { ctrlKey: true },
    { shiftKey: true },
    { altKey: true },
    { button: 1 },
  ]) {
    const page = createPage();
    const beforeLabel = page.action.textContent;
    const result = click(page, event);
    assert.equal(result.defaultPrevented, false);
    assert.equal(page.elements.get("install-progress").hidden, true);
    assert.equal(page.action.textContent, beforeLabel);
    assert.equal(page.fetchCalls.length, 0);
  }
});

test("status phases render server bytes and completion without claiming installation", async () => {
  const page = createPage({ platform: "ios" });
  const phases = [
    ["waiting", 0, /confirm on your device/i],
    ["preparing", 0, /preparing download/i],
    ["transferring", 250, /transferring package/i],
    ["interrupted", 250, /transfer interrupted/i],
    ["transferred", 1000, /package transfer complete/i],
  ];
  for (const [phase, bytesSent] of phases) page.setFetchResponse(jsonResponse(installableState(phase, bytesSent)));

  await startAndPoll(page);
  const heading = page.elements.get("progress-heading");
  const detail = page.elements.get("progress-detail");
  const bar = page.elements.get("progress-bar");
  const amount = page.elements.get("progress-amount");
  assert.match(heading.textContent, phases[0][2]);

  await page.runNextTimer();
  assert.match(heading.textContent, phases[1][2]);
  await page.runNextTimer();
  assert.match(heading.textContent, phases[2][2]);
  assert.equal(bar.value, 25);
  assert.match(amount.textContent, /250 B/);
  assert.match(amount.textContent, /1000 B/);

  await page.runNextTimer();
  assert.match(heading.textContent, phases[3][2]);
  assert.equal(isDisabled(page.action), false);
  await page.runNextTimer();
  assert.match(heading.textContent, phases[4][2]);
  assert.equal(bar.value, 100);
  assert.equal(isDisabled(page.action), false);
  assert.doesNotMatch(heading.textContent, /installed/i);
  assert.doesNotMatch(detail.textContent, /installed/i);
});

test("an active transfer remains truthful when availability changes", async () => {
  const page = createPage();
  page.setFetchResponse(jsonResponse({
    ...installableState("transferring", 500),
    availability: "expired",
  }));

  await startAndPoll(page);
  assert.equal(page.elements.get("progress-bar").value, 50);
  assert.match(page.elements.get("progress-heading").textContent, /transferring package/i);
  assert.doesNotMatch(page.elements.get("progress-heading").textContent, /expired|limit|ended|failed/i);
  assert.equal(isDisabled(page.action), true);
});

test("network errors and request timeouts re-enable a recoverable CTA", async () => {
  const networkPage = createPage();
  networkPage.setFetchError();
  await startAndPoll(networkPage);
  assert.match(networkPage.elements.get("progress-heading").textContent, /progress unavailable/i);
  assert.equal(isDisabled(networkPage.action), false);

  const timeoutPage = createPage();
  timeoutPage.setPendingFetch();
  click(timeoutPage);
  await timeoutPage.runNextTimer();
  assert.equal(timeoutPage.fetchCalls.length, 1);
  await timeoutPage.runNextTimer({ at: 8000 });
  assert.equal(timeoutPage.fetchCalls[0].options.signal.aborted, true);
  assert.match(timeoutPage.elements.get("progress-heading").textContent, /progress unavailable/i);
  assert.equal(isDisabled(timeoutPage.action), false);
});

test("visibility pauses polling and resumes it after the page is visible", async () => {
  const page = createPage();
  page.setFetchResponse(jsonResponse(installableState("waiting", 0)));
  await startAndPoll(page);
  assert.equal(page.fetchCalls.length, 1);

  page.setFetchResponse(jsonResponse(installableState("transferring", 100)));
  page.document.hidden = true;
  page.document.dispatch("visibilitychange");
  assert.equal(page.timers.size, 0);
  page.document.hidden = false;
  page.document.dispatch("visibilitychange");
  await page.runNextTimer();
  assert.equal(page.fetchCalls.length, 2);
  assert.equal(page.elements.get("progress-bar").value, 10);
});

test("waiting and stalled transfers offer retry without inventing progress", async () => {
  const waitingPage = createPage();
  waitingPage.setFetchResponse(jsonResponse(installableState("waiting", 0)));
  await startAndPoll(waitingPage);
  waitingPage.setFetchResponse(jsonResponse(installableState("waiting", 0)));
  await waitingPage.runNextTimer({ at: 16_000 });
  assert.match(waitingPage.elements.get("progress-heading").textContent, /download has not started/i);
  assert.equal(isDisabled(waitingPage.action), false);
  assert.equal(waitingPage.elements.get("progress-bar").hidden, true);
  assert.equal(waitingPage.elements.get("progress-amount").hidden, true);

  const stalledPage = createPage();
  stalledPage.setFetchResponse(jsonResponse(installableState("transferring", 100)));
  await startAndPoll(stalledPage);
  stalledPage.setFetchResponse(jsonResponse(installableState("transferring", 100)));
  await stalledPage.runNextTimer({ at: 21_000 });
  assert.match(stalledPage.elements.get("progress-heading").textContent, /waiting for transfer to continue/i);
  assert.equal(isDisabled(stalledPage.action), false);
  assert.equal(stalledPage.elements.get("progress-bar").value, 10);
  assert.match(stalledPage.elements.get("progress-amount").textContent, /100 B/);
});

test("repeated status failures end in a reload action, while a later network recovery completes", async () => {
  const terminalPage = createPage();
  for (let index = 0; index < 12; index += 1) terminalPage.setFetchError();
  await startAndPoll(terminalPage);
  for (let index = 1; index < 12; index += 1) await terminalPage.runNextTimer();
  assert.equal(terminalPage.fetchCalls.length, 12);
  assert.match(terminalPage.elements.get("progress-heading").textContent, /progress unavailable/i);
  assert.equal(terminalPage.action.textContent, "Reload install page");
  assert.equal(isDisabled(terminalPage.action), false);
  assert.equal(terminalPage.action.href, "https://example.test/install/app-1");
  assert.equal(terminalPage.timers.size, 0, "terminal failure must stop polling");

  const recoveryPage = createPage();
  recoveryPage.setFetchError();
  recoveryPage.setFetchResponse(jsonResponse(installableState("transferred", 1000)));
  await startAndPoll(recoveryPage);
  assert.equal(isDisabled(recoveryPage.action), false);
  await recoveryPage.runNextTimer();
  assert.match(recoveryPage.elements.get("progress-heading").textContent, /package transfer complete/i);
  assert.doesNotMatch(recoveryPage.elements.get("progress-detail").textContent, /installed/i);
});

test("Android completion tells the user where to open the APK", async () => {
  const page = createPage({ platform: "android" });
  page.setFetchResponse(jsonResponse(installableState("transferred", 1000)));
  await startAndPoll(page);
  assert.match(page.elements.get("progress-heading").textContent, /package transfer complete/i);
  assert.match(page.elements.get("progress-detail").textContent, /open the apk from your browser.s downloads/i);
  assert.doesNotMatch(page.elements.get("progress-detail").textContent, /home screen/i);
});

test("an expired or stale grant becomes a reloadable page instead of a stuck disabled CTA", async () => {
  const statusPage = createPage();
  statusPage.setFetchResponse(jsonResponse({
    ...installableState("waiting", 0),
    availability: "expired",
  }));
  await startAndPoll(statusPage);
  assert.match(statusPage.elements.get("progress-heading").textContent, /link expired/i);
  assert.equal(isDisabled(statusPage.action), false);
  assert.equal(statusPage.action.href, "https://example.test/install/app-1");

  const stalePage = createPage();
  stalePage.setFetchResponse(jsonResponse({}, 410));
  await startAndPoll(stalePage);
  assert.match(stalePage.elements.get("progress-heading").textContent, /progress session ended/i);
  assert.equal(isDisabled(stalePage.action), false);
  assert.equal(stalePage.action.href, "https://example.test/install/app-1");
});
