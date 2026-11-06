// tests/js/asn_lookup.test.js — behavioural tests for the client-IP
// lookup of the admin UI (issue #134).
//
// The admin UI has no JavaScript test runner, so this file drives
// `assets/app.js` itself: the script is evaluated in a `vm` context
// with a minimal DOM stub, and the handlers it registers are driven
// the way a browser would drive them. That keeps the test honest — it
// exercises the file that is served to the operator, not a copy of
// its logic.
//
// Run with: node --test tests/js/

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const APP_JS = path.join(__dirname, '..', '..', 'assets', 'app.js');

/// The tooltip the templates put on the button.
const LOOKUP_TITLE =
  'lookup IP info — sends this address to ipwhois.app / ipinfo.io';

/// A minimal class list.
function classList(classes) {
  const set = new Set(classes);
  return {
    add: (c) => set.add(c),
    remove: (c) => set.delete(c),
    contains: (c) => set.has(c),
    toggle: (c, force) => {
      const on = force === undefined ? !set.has(c) : !!force;
      if (on) set.add(c);
      else set.delete(c);
      return on;
    },
  };
}

/// A minimal element: a dataset, a class list, a title and a text.
function el(attrs = {}, classes = [], matches = () => false) {
  return {
    dataset: attrs.dataset || {},
    classList: classList(classes),
    title: attrs.title || '',
    disabled: false,
    textContent: '',
    _matches: matches,
    _children: [],
    _parent: null,
    querySelector(sel) {
      const attr = sel.replace(/[[\]]/g, '');
      return this._children.find((c) => c._matches(attr)) || null;
    },
    closest(sel) {
      const attr = sel.replace(/[[\]]/g, '');
      let node = this;
      while (node) {
        if (node._matches(attr)) return node;
        node = node._parent;
      }
      return null;
    },
  };
}

/// The markup the templates render: the cell, the button, the
/// magnifier, the spinner and the two result placeholders.
function cell(ip) {
  const c = el({ dataset: { asnIp: ip } });
  const btn = el({ title: LOOKUP_TITLE }, [], (a) => a === 'data-asn-trigger');
  const icon = el({}, [], (a) => a === 'data-asn-icon');
  const spinner = el({}, ['hidden'], (a) => a === 'data-asn-spinner');
  const geo = el({}, ['hidden'], (a) => a === 'data-asn-geo');
  const asn = el({}, ['hidden'], (a) => a === 'data-asn-asn');
  btn._children = [icon, spinner];
  c._children = [btn, geo, asn];
  btn._parent = c;
  c._matches = (a) => a === 'data-asn-ip';
  return Object.assign(c, { btn, icon, spinner, geo, asn });
}

/// Load `assets/app.js` with a stubbed DOM. `fetchImpl` maps a URL to
/// a function returning a promise; an unknown URL is a network error.
function loadApp({ cells = [], store = {}, fetchImpl = () => null } = {}) {
  const handlers = {};
  const requests = [];
  const document = {
    readyState: 'complete',
    addEventListener(type, fn) {
      (handlers[type] = handlers[type] || []).push(fn);
    },
    querySelectorAll(sel) {
      return sel === '[data-asn-ip]' ? cells : [];
    },
    querySelector() {
      return null;
    },
    getElementById() {
      return null;
    },
  };
  const sandbox = {
    document,
    console,
    Promise,
    JSON,
    Date,
    Error,
    String,
    setTimeout,
    clearTimeout,
    AbortController,
    fetch(url) {
      requests.push(url);
      const impl = fetchImpl(url);
      return impl ? impl() : Promise.reject(new Error('network error'));
    },
    window: {
      setTimeout,
      clearTimeout,
      localStorage: {
        getItem: (k) => (k in store ? store[k] : null),
        setItem: (k, v) => {
          store[k] = String(v);
        },
      },
    },
  };
  sandbox.window.document = document;
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(APP_JS, 'utf8'), sandbox);
  return { handlers, requests, store };
}

/// A `fetch` answer.
function json(body, ok = true, status = 200) {
  return () =>
    Promise.resolve({
      ok,
      status,
      json: () => Promise.resolve(body),
    });
}

/// Click the magnifier and let the promise chain settle.
async function click(app, target) {
  const evt = { target: target.btn, preventDefault() {} };
  for (const h of app.handlers.click || []) h(evt);
  // The whole lookup runs on microtasks, so one macrotask turn is
  // enough to drain it.
  await new Promise((r) => setImmediate(r));
}

/// Swap the table region the way htmx does.
function swap(app) {
  for (const h of app.handlers['htmx:afterSwap'] || []) {
    h({ detail: { target: { id: '' } } });
  }
}

test('the first provider answers and the second is not asked', async () => {
  const c = cell('45.138.12.89');
  const app = loadApp({
    cells: [c],
    fetchImpl: (url) =>
      url === 'https://ipwhois.app/json/45.138.12.89'
        ? json({
            success: true,
            asn: 15169,
            org: 'Google LLC',
            city: 'Mountain View',
            region: 'California',
            country: 'United States',
            country_code: 'US',
          })
        : null,
  });

  await click(app, c);

  assert.deepEqual(app.requests, ['https://ipwhois.app/json/45.138.12.89']);
  assert.equal(c.geo.textContent, 'Mountain View, California, United States');
  assert.equal(c.asn.textContent, 'AS15169 Google LLC');
  assert.equal(c.geo.classList.contains('hidden'), false);
  assert.equal(c.asn.classList.contains('hidden'), false);
  // The provider is shown with the answer.
  assert.match(c.asn.title, /ipwhois\.app/);
  // The button is gone — there is nothing left to click.
  assert.equal(c.btn.classList.contains('hidden'), true);
  // The spinner is not left running.
  assert.equal(c.spinner.classList.contains('hidden'), true);
});

test('a provider that declines falls through to the next one', async () => {
  const c = cell('1.2.3.4');
  const app = loadApp({
    cells: [c],
    fetchImpl: (url) =>
      url === 'https://ipwhois.app/json/1.2.3.4'
        ? json({ success: false })
        : json({
            city: 'Berlin',
            region: 'Berlin',
            country: 'DE',
            org: 'AS15169 Google LLC',
          }),
  });

  await click(app, c);

  assert.deepEqual(app.requests, [
    'https://ipwhois.app/json/1.2.3.4',
    'https://ipinfo.io/1.2.3.4/json',
  ]);
  assert.equal(c.geo.textContent, 'Berlin, Berlin, DE');
  assert.equal(c.asn.textContent, 'AS15169 Google LLC');
});

test('both providers declining leaves the button and remembers the failure', async () => {
  const c = cell('10.0.0.1');
  const app = loadApp({
    cells: [c],
    fetchImpl: (url) =>
      url === 'https://ipwhois.app/json/10.0.0.1'
        ? json({ success: false })
        : json({}, false, 400),
  });

  await click(app, c);

  assert.equal(app.requests.length, 2);
  assert.equal(c.geo.textContent, '');
  assert.equal(c.asn.textContent, '');
  assert.equal(c.geo.classList.contains('hidden'), true);
  assert.equal(c.asn.classList.contains('hidden'), true);
  assert.equal(c.btn.classList.contains('hidden'), false);
  assert.equal(c.btn.title, 'lookup failed — click to retry');
  const cache = JSON.parse(app.store['pangolin-asn-cache-v1']);
  assert.equal(cache['10.0.0.1'].failed, true);
});

test('an organisation without an ASN is not turned into one', async () => {
  const c = cell('9.9.9.9');
  const app = loadApp({
    cells: [c],
    // The first provider declines, so the answer comes from ipinfo.io
    // — the provider whose `org` field is split on its first space.
    fetchImpl: (url) =>
      url.startsWith('https://ipwhois.app/')
        ? json({ success: false })
        : json({ org: 'Google LLC', country: 'US' }),
  });

  await click(app, c);

  assert.equal(app.requests.length, 2);
  // "Google" is not an ASN — the whole organisation is the name.
  assert.equal(c.asn.textContent, 'Google LLC');
  assert.equal(c.asn.textContent.includes('ASGoogle'), false);
});

test('a cached answer renders without a network request', async () => {
  const c = cell('8.8.8.8');
  const store = {
    'pangolin-asn-cache-v1': JSON.stringify({
      '8.8.8.8': {
        asn: 'AS15169',
        org: 'Google LLC',
        city: 'Mountain View',
        region: 'California',
        countryName: 'United States',
        countryCode: 'US',
        provider: 'ipwhois.app',
        ts: Date.now(),
      },
    }),
  };
  const app = loadApp({ cells: [c], store });

  await click(app, c);

  assert.deepEqual(app.requests, []);
  assert.equal(c.geo.textContent, 'Mountain View, California, United States');
  assert.equal(c.asn.textContent, 'AS15169 Google LLC');
});

test('a cached answer older than a day is looked up again', async () => {
  const c = cell('8.8.8.8');
  const store = {
    'pangolin-asn-cache-v1': JSON.stringify({
      '8.8.8.8': { asn: 'AS1', org: '', city: '', region: '', ts: Date.now() - 25 * 3600 * 1000 },
    }),
  };
  const app = loadApp({
    cells: [c],
    store,
    fetchImpl: () => json({ success: true, asn: 15169, org: 'Google LLC' }),
  });

  await click(app, c);

  assert.equal(app.requests.length, 1);
});

test('the answer survives the refresh of the table', async () => {
  const cells = [cell('8.8.8.8')];
  const app = loadApp({
    cells,
    fetchImpl: () =>
      json({ success: true, asn: 15169, org: 'Google LLC', city: 'Mountain View' }),
  });
  await click(app, cells[0]);

  // htmx replaces the region wholesale every two seconds — the cell
  // is a new one, and the answer has to come back from the cache.
  const fresh = cell('8.8.8.8');
  cells[0] = fresh;
  swap(app);

  assert.equal(fresh.geo.textContent, 'Mountain View');
  assert.equal(fresh.asn.textContent, 'AS15169 Google LLC');
  assert.equal(fresh.btn.classList.contains('hidden'), true);
  // The refresh must not trigger a lookup of its own.
  assert.equal(app.requests.length, 1);
});

test('the button is disabled while the lookup runs', async () => {
  const c = cell('5.5.5.5');
  let release;
  const pending = new Promise((r) => {
    release = r;
  });
  const app = loadApp({
    cells: [c],
    fetchImpl: () => () => pending.then(() => ({ ok: true, json: () => ({}) })),
  });

  const evt = { target: c.btn, preventDefault() {} };
  for (const h of app.handlers.click) h(evt);

  assert.equal(c.btn.disabled, true);
  assert.equal(c.icon.classList.contains('hidden'), true);
  assert.equal(c.spinner.classList.contains('hidden'), false);

  release();
  await new Promise((r) => setImmediate(r));
  assert.equal(c.btn.disabled, false);
  assert.equal(c.spinner.classList.contains('hidden'), true);
});
