// tests/js/lang_switcher.test.js — behavioural tests for the language
// switcher of the admin UI (issue #178).
//
// The admin UI has no JavaScript test runner, so this file drives
// `assets/app.js` itself: the script is evaluated in a `vm` context
// with a minimal DOM stub, and the code it runs at load time is
// driven the way a browser would drive it. That keeps the test
// honest — it exercises the file that is served to the operator, not
// a copy of its logic.
//
// The switcher is a plain `<form method="GET" action="/lang/{code}">`
// with a hidden `next` field. The field tells the server where to
// send the operator back to; the script fills it with the current
// page.
//
// Run with: node --test tests/js/

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const APP_JS = path.join(__dirname, '..', '..', 'assets', 'app.js');

/// Load `assets/app.js` with a stubbed DOM.
///
/// `switcherInputs` are the hidden `next` fields of the switcher's
/// forms; `location` is the page the browser is on.
function loadApp({ switcherInputs = [], location = { pathname: '/', search: '' } } = {}) {
  const document = {
    readyState: 'complete',
    addEventListener() {},
    querySelector(sel) {
      return null;
    },
    querySelectorAll(sel) {
      if (sel === 'input[data-lang-next]') return switcherInputs;
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
      scrollX: 0,
      scrollY: 0,
      innerWidth: 1024,
      location,
      localStorage: {
        getItem: () => null,
        setItem: () => {},
      },
    },
  };
  sandbox.window.document = document;
  vm.createContext(sandbox);
  vm.runInContext(fs.readFileSync(APP_JS, 'utf8'), sandbox);
}

test('the switcher fills the next field with the current page', () => {
  const input = { value: '' };
  loadApp({
    switcherInputs: [input],
    location: { pathname: '/sites', search: '?page=2' },
  });
  assert.equal(input.value, '/sites?page=2');
});

test('the switcher fills every next field it finds', () => {
  const first = { value: '' };
  const second = { value: '' };
  loadApp({
    switcherInputs: [first, second],
    location: { pathname: '/certs', search: '' },
  });
  assert.equal(first.value, '/certs');
  assert.equal(second.value, '/certs');
});

test('the switcher leaves the page alone when there is none', () => {
  // No switcher on the page — the login page has none, and the
  // script must not fail there.
  loadApp({ location: { pathname: '/login', search: '' } });
});

test('the script reads its strings from the server-rendered dictionary', () => {
  // The dictionary the base layout writes into the page.
  const source = fs.readFileSync(APP_JS, 'utf8');
  assert.ok(
    source.includes('window.__PANGOLIN_I18N'),
    'the script must read the dictionary the server rendered'
  );
  // The four strings the issue lists must come from the dictionary,
  // not from a literal in the script.
  for (const key of [
    'js.credentials_verified',
    'js.request_failed',
    'js.empty',
    'js.backend_required',
  ]) {
    assert.ok(source.includes(key), `the script must read ${key} from the dictionary`);
  }
});
