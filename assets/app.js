// assets/app.js — Pangolin admin UI client runtime.
//
// Single bundle loaded once from base.html with defer and ?v=<hash>.
// Provides all interactive behaviour for the admin panels:
//
//   - mobile nav toggle (#menu-toggle → #mobile-nav)
//   - DNS provider kind radiogroup → toggles [data-kind-panel] visibility
//   - secret-field show/hide (data-toggle-password=<input id>)
//   - secret-field mask/replace (data-show-replace, data-show-mask)
//   - DNS provider Test connection (data-test-connection → /admin/dns/test)
//   - htmx modal lifecycle (#modal + #modal-body swap)
//   - htmx toast auto-clear (#toast swap, 4s timeout)
//
// htmx is loaded as a sibling ES module so it installs on window before
// any of these listeners run (modules execute in order).
import './vendor/htmx-1.9.0.min.js';

(function () {
  'use strict';

  // ── Mobile nav toggle ─────────────────────────────────────────────────
  document.addEventListener('click', function (e) {
    var btn = e.target.closest('#menu-toggle');
    if (!btn) return;
    var nav = document.getElementById('mobile-nav');
    if (nav) nav.classList.toggle('hidden');
  });

  // ── DNS kind radiogroup sync ──────────────────────────────────────────
  function syncKindPanels() {
    var group = document.querySelector('[data-kind-group]');
    if (!group) return;
    var cur = group.querySelector('input[name="kind"]:checked');
    var v = cur ? cur.value : null;
    document.querySelectorAll('[data-kind-panel]').forEach(function (p) {
      p.classList.toggle('hidden', p.dataset.kindPanel !== v);
    });
  }
  document.addEventListener('change', function (e) {
    if (e.target.matches('[data-kind-group] input[name="kind"]')) {
      syncKindPanels();
    }
  });
  syncKindPanels();

  // ── Secret-field show/hide ────────────────────────────────────────────
  document.addEventListener('click', function (e) {
    var btn = e.target.closest('[data-toggle-password]');
    if (!btn) return;
    var el = document.getElementById(btn.dataset.togglePassword);
    if (!el) return;
    el.type = el.type === 'password' ? 'text' : 'password';
  });

  // ── Secret-field mask ↔ replace toggle ────────────────────────────────
  document.addEventListener('click', function (e) {
    if (e.target.closest('[data-show-replace]')) {
      var mask = e.target.closest('[data-secret-mask]');
      if (!mask) return;
      mask.querySelector('[data-mask-view]').classList.add('hidden');
      mask.querySelector('[data-replace-view]').classList.remove('hidden');
    }
    if (e.target.closest('[data-show-mask]')) {
      var mask2 = e.target.closest('[data-secret-mask]');
      if (!mask2) return;
      mask2.querySelector('[data-mask-view]').classList.remove('hidden');
      mask2.querySelector('[data-replace-view]').classList.add('hidden');
      var input = mask2.querySelector('input[name]');
      if (input) input.value = '';
    }
  });

  // ── DNS Test connection ───────────────────────────────────────────────
  document.addEventListener('click', function (e) {
    var btn = e.target.closest('[data-test-connection]');
    if (!btn) return;
    e.preventDefault();
    var form = btn.closest('form');
    if (!form) return;
    var fd = new FormData(form);
    var resultEl = document.getElementById('test-result');
    if (resultEl) {
      resultEl.innerHTML =
        '<span class="text-slate-500">Verifying…</span>';
    }
    btn.disabled = true;
    fetch('/admin/dns/test', {
      method: 'POST',
      body: fd,
      credentials: 'same-origin',
    })
      .then(function (r) {
        return r.json();
      })
      .then(function (j) {
        if (!resultEl) return;
        if (j.ok) {
          resultEl.innerHTML =
            '<div class="rounded-lg border-l-4 border-emerald-500 bg-emerald-50 dark:bg-emerald-900/20 px-4 py-2 text-sm text-emerald-900 dark:text-emerald-100">✓ Credentials verified</div>';
        } else {
          resultEl.innerHTML =
            '<div class="rounded-lg border-l-4 border-red-500 bg-red-50 dark:bg-red-900/20 px-4 py-2 text-sm text-red-900 dark:text-red-100">✗ ' +
            (j.error || 'Verification failed') +
            '</div>';
        }
      })
      .catch(function (err) {
        if (!resultEl) return;
        resultEl.innerHTML =
          '<div class="rounded-lg border-l-4 border-red-500 bg-red-50 dark:bg-red-900/20 px-4 py-2 text-sm text-red-900 dark:text-red-100">✗ ' +
          err.message +
          '</div>';
      })
      .finally(function () {
        btn.disabled = false;
      });
  });

  // ── htmx modal + toast helpers (used by site_domains.html) ────────────
  document.addEventListener('htmx:afterSwap', function (e) {
    var modal = document.getElementById('modal');
    if (modal && e.detail.target.id === 'modal-body') {
      modal.showModal();
    }
    if (e.detail.target.id === 'toast' && e.detail.target.innerHTML.trim()) {
      setTimeout(function () {
        e.detail.target.innerHTML = '';
      }, 4000);
    }
  });
  document.addEventListener('click', function (e) {
    var m = e.target;
    if (m && m.id === 'modal') m.close();
  });
})();