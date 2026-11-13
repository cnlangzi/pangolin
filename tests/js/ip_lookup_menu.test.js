// tests/js/ip_lookup_menu.test.js — behavioural tests for the IP-lookup
// menu of the admin UI (issue #149).
//
// The admin UI has no JavaScript test runner, so this file drives
// `assets/app.js` itself: the script is evaluated in a `vm` context
// with a minimal DOM stub, and the handlers it registers are driven
// the way a browser would drive them. That keeps the test honest — it
// exercises the file that is served to the operator, not a copy of
// its logic.
//
// The DOM stub models the shape that matters for this feature:
//
//   body
//     ├─ region      ← the table, replaced wholesale every 2 s
//     │    └─ the caret button of a row
//     └─ panel       ← the menu, a child of `<body>`
//
// The panel is outside the region on purpose: the region is swapped
// by htmx, and a menu inside it would be destroyed while the
// operator reads it.
//
// Run with: node --test tests/js/

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const APP_JS = path.join(__dirname, '..', '..', 'assets', 'app.js');

/// The URL templates the layout renders — one entry per site.
const SITES = [
  ['https://ipinsights.io/ip/{ip}', 'ipinsights.io'],
  ['https://viz.greynoise.io/ips/{ip}', 'GreyNoise'],
  ['https://www.abuseipdb.com/check/{ip}', 'AbuseIPDB'],
  ['https://scamalytics.com/ip/{ip}', 'Scamalytics'],
];

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

/// A minimal element: attributes, a class list, a style map, a
/// bounding rectangle and a tree of children.
///
/// `matches` holds the attribute names the element answers to — the
/// stub's stand-in for a CSS selector.
function makeEl({
  attrs = {},
  classes = [],
  matches = [],
  rect = { left: 0, top: 0, right: 0, bottom: 0 },
  width = 0,
} = {}) {
  const el = {
    attrs: Object.assign({}, attrs),
    classList: classList(classes),
    style: {},
    matches: new Set(matches),
    children: [],
    parent: null,
    rect,
    // The panel is measured after it is shown; every other element
    // has no width.
    offsetWidth: width,
    // Whether the node is still in the document — the tables'
    // refresh detaches the rows it replaces.
    isConnected: true,
    getAttribute(name) {
      return name in this.attrs ? this.attrs[name] : null;
    },
    setAttribute(name, value) {
      this.attrs[name] = String(value);
    },
    removeAttribute(name) {
      delete this.attrs[name];
    },
    getBoundingClientRect() {
      return this.rect;
    },
    querySelectorAll(sel) {
      const name = sel.replace(/[[\]]/g, '');
      return this.children.filter((c) => c.matches.has(name));
    },
    querySelector(sel) {
      const name = sel.replace(/[[\]]/g, '');
      return this.children.find((c) => c.matches.has(name)) || null;
    },
    closest(sel) {
      const name = sel.replace(/[[\]]/g, '');
      let node = this;
      while (node) {
        if (node.matches.has(name)) return node;
        node = node.parent;
      }
      return null;
    },
  };
  return el;
}

/// The caret button of a row.
function toggleButton(ip, rect) {
  return makeEl({
    attrs: { 'data-ip': ip, 'aria-expanded': 'false' },
    matches: ['data-ip-menu-toggle'],
    rect,
  });
}

/// The panel and its four entries — the markup the layout renders.
function menu({ width = 0 } = {}) {
  const panel = makeEl({
    classes: ['hidden'],
    matches: ['data-ip-menu-panel'],
    width,
  });
  const entries = SITES.map(([template]) => {
    const entry = makeEl({
      attrs: {
        'data-url-template': template,
        // The markup the layout renders: the entry opens in a new tab
        // and hands the opened page no reference to the admin UI.
        target: '_blank',
        rel: 'noopener noreferrer',
      },
      matches: ['data-ip-site'],
    });
    entry.parent = panel;
    panel.children.push(entry);
    return entry;
  });
  return { panel, entries };
}

/// Load `assets/app.js` with a stubbed DOM.
function loadApp({
  rows = [],
  scrollX = 0,
  scrollY = 0,
  withPanel = true,
  panelWidth = 0,
  innerWidth = 1024,
} = {}) {
  const handlers = {};
  const { panel, entries } = menu({ width: panelWidth });

  // The region htmx replaces wholesale every two seconds. The
  // panel is its sibling — a child of `<body>`, outside the region.
  const region = makeEl({ matches: ['data-region'] });
  const body = makeEl({ matches: ['data-body'] });
  body.children = [region, panel];

  for (const row of rows) {
    region.children.push(row);
    row.parent = region;
  }

  const document = {
    readyState: 'complete',
    addEventListener(type, fn) {
      (handlers[type] = handlers[type] || []).push(fn);
    },
    querySelector(sel) {
      const name = sel.replace(/[[\]]/g, '');
      if (name === 'data-ip-menu-panel') return withPanel ? panel : null;
      return null;
    },
    querySelectorAll(sel) {
      const name = sel.replace(/[[\]]/g, '');
      if (name === 'data-ip-site') return withPanel ? entries : [];
      return [];
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
    fetch() {
      return Promise.reject(new Error('network error'));
    },
    window: {
      setTimeout,
      clearTimeout,
      scrollX,
      scrollY,
      innerWidth,
      localStorage: {
        getItem: () => null,
        setItem: () => {},
      },
    },
  };
  sandbox.window.document = document;
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(APP_JS, 'utf8'), sandbox);

  return { handlers, panel, entries, region, body };
}

/// Click something.
function click(app, target) {
  const evt = { target, preventDefault() {} };
  for (const h of app.handlers.click || []) h(evt);
}

/// Press a key.
function press(app, key) {
  const evt = { key };
  for (const h of app.handlers.keydown || []) h(evt);
}

/// Swap the table region the way htmx does: the region is replaced
/// wholesale, so the caret button inside it is gone — and the node
/// that carried it is no longer in the document.
function swap(app) {
  for (const child of app.region.children) {
    child.isConnected = false;
  }
  app.region.children = [];
  for (const h of app.handlers['htmx:afterSwap'] || []) {
    h({ detail: { target: { id: '' } } });
  }
}

test('the menu opens with the lookup sites of the row', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);

  assert.equal(app.panel.classList.contains('hidden'), false);
  assert.equal(app.panel.getAttribute('data-ip'), '198.51.100.9');
  assert.equal(btn.getAttribute('aria-expanded'), 'true');
  // Every entry points at its own site, for this address.
  assert.equal(app.entries.length, 4);
  app.entries.forEach((entry, i) => {
    assert.equal(
      entry.getAttribute('href'),
      SITES[i][0].replace('{ip}', '198.51.100.9')
    );
    // The page opens in a new tab and gets no reference back to the
    // admin UI.
    assert.equal(entry.getAttribute('target'), '_blank');
    assert.equal(entry.getAttribute('rel'), 'noopener noreferrer');
  });
});

test('the same button closes the menu again', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), false);

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), true);
  assert.equal(btn.getAttribute('aria-expanded'), 'false');
});

test('a click outside the menu closes it', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), false);

  // Something else on the page — not the panel, not the button.
  click(app, makeEl({ matches: ['data-elsewhere'] }));
  assert.equal(app.panel.classList.contains('hidden'), true);
  assert.equal(btn.getAttribute('aria-expanded'), 'false');
});

test('choosing an entry closes the menu', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), false);

  // The entry opens its site in a new tab — the menu has done its
  // job and goes away.
  click(app, app.entries[0]);
  assert.equal(app.panel.classList.contains('hidden'), true);
});

test('Esc closes the menu', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), false);

  press(app, 'Escape');
  assert.equal(app.panel.classList.contains('hidden'), true);
  assert.equal(btn.getAttribute('aria-expanded'), 'false');
});

test('opening the menu of another row closes the first one', () => {
  const first = toggleButton('198.51.100.9');
  const second = toggleButton('203.0.113.7');
  const app = loadApp({ rows: [first, second] });

  click(app, first);
  click(app, second);

  // One panel, one address: the menu follows the row it was opened
  // from.
  assert.equal(app.panel.classList.contains('hidden'), false);
  assert.equal(app.panel.getAttribute('data-ip'), '203.0.113.7');
  assert.equal(app.entries[0].getAttribute('href'), SITES[0][0].replace('{ip}', '203.0.113.7'));
  // The button of the first row is closed again.
  assert.equal(first.getAttribute('aria-expanded'), 'false');
  assert.equal(second.getAttribute('aria-expanded'), 'true');
});

test('the panel is positioned under the button', () => {
  const rect = { left: 120, top: 40, right: 140, bottom: 56 };
  const btn = toggleButton('198.51.100.9', rect);
  const app = loadApp({ rows: [btn], scrollX: 10, scrollY: 200 });

  click(app, btn);

  // The viewport rectangle plus the page scroll offset — the panel
  // is positioned absolutely, so it has to follow the document.
  assert.equal(app.panel.style.left, '130px');
  assert.equal(app.panel.style.top, '256px');
});

test('the menu survives the refresh of the table', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  assert.equal(app.panel.classList.contains('hidden'), false);

  // htmx replaces the table region wholesale — the caret button is
  // gone, but the panel lives outside the region and keeps the
  // address it was opened with.
  swap(app);
  assert.equal(app.region.children.length, 0);
  assert.equal(app.panel.classList.contains('hidden'), false);
  assert.equal(app.panel.getAttribute('data-ip'), '198.51.100.9');
  assert.equal(
    app.entries[0].getAttribute('href'),
    SITES[0][0].replace('{ip}', '198.51.100.9')
  );
});

test('the caret of the same row closes the menu after a refresh', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  swap(app);

  // The row was replaced — the new caret is a *different* node, but
  // the menu is still open for that address, so the caret of that
  // address closes it.
  const fresh = toggleButton('198.51.100.9');
  app.region.children.push(fresh);
  fresh.parent = app.region;

  click(app, fresh);
  assert.equal(app.panel.classList.contains('hidden'), true);
});

test('the caret of another row moves the menu after a refresh', () => {
  const btn = toggleButton('198.51.100.9');
  const app = loadApp({ rows: [btn] });

  click(app, btn);
  swap(app);

  // A different address: the menu follows the caret it was opened
  // from, it does not close.
  const fresh = toggleButton('203.0.113.7');
  app.region.children.push(fresh);
  fresh.parent = app.region;

  click(app, fresh);
  assert.equal(app.panel.classList.contains('hidden'), false);
  assert.equal(app.panel.getAttribute('data-ip'), '203.0.113.7');
});

test('the panel is kept on the screen', () => {
  // A caret near the right edge — the tables scroll sideways, so
  // that happens.
  const rect = { left: 450, top: 40, right: 470, bottom: 56 };
  const btn = toggleButton('198.51.100.9', rect);
  const app = loadApp({
    rows: [btn],
    panelWidth: 208,
    innerWidth: 500,
  });

  click(app, btn);

  // The panel is 208 wide and the viewport 500 — it must not hang
  // over the right edge.
  assert.equal(app.panel.style.left, '284px');
  assert.equal(app.panel.style.top, '56px');
});

test('a page without the panel breaks nothing', () => {
  const btn = toggleButton('198.51.100.9');
  // The panel is gone — the layout did not render it.
  const app = loadApp({ rows: [btn], withPanel: false });

  click(app, btn);
  click(app, btn);
  assert.equal(btn.getAttribute('aria-expanded'), 'false');
});
