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
//   - site form backend mode toggle (data-backend-form)
//
// htmx is loaded separately via script tag in base.html

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
      var isActive = p.dataset.kindPanel === v;
      p.classList.toggle('hidden', !isActive);
      // Disable inputs in inactive panels so:
      //   (1) browsers don't block submit with `required` fields the user
      //       can't see ("invalid form control is not focusable"), and
      //   (2) FormData / URLSearchParams don't pick up stale hidden values
      //       when the user switches provider kind mid-form.
      p.querySelectorAll('input, select, textarea').forEach(function (el) {
        // Preserve the original disabled state set by the template (e.g.
        // `disabled` on kind radios in edit mode) — toggle from there.
        if (el.dataset.originalDisabled === undefined) {
          el.dataset.originalDisabled = el.disabled ? '1' : '0';
        }
        el.disabled = !isActive || el.dataset.originalDisabled === '1';
      });
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
  // Build the result banner with DOM APIs (createElement + textContent)
  // rather than innerHTML, because the user-supplied error string can come
  // straight from the server's JSON body and we never want it parsed as
  // markup.
  function setTestResult(resultEl, kind, msg) {
    if (!resultEl) return;
    var box = document.createElement('div');
    box.className =
      kind === 'ok'
        ? 'rounded-lg border-l-4 border-emerald-500 bg-emerald-50 dark:bg-emerald-900/20 px-4 py-2 text-sm text-emerald-900 dark:text-emerald-100'
        : 'rounded-lg border-l-4 border-red-500 bg-red-50 dark:bg-red-900/20 px-4 py-2 text-sm text-red-900 dark:text-red-100';
    box.appendChild(document.createTextNode(kind === 'ok' ? '✓ ' : '✗ '));
    // msg may be undefined → render an empty suffix without crashing.
    box.appendChild(document.createTextNode(msg != null ? String(msg) : ''));
    resultEl.replaceChildren(box);
  }

  document.addEventListener('click', function (e) {
    var btn = e.target.closest('[data-test-connection]');
    if (!btn) return;
    e.preventDefault();
    // The Test button lives OUTSIDE the create form (so the page only has
    // a single submit-capable form and Chrome stops warning about
    // "multiple forms"). Scope the lookup to the same <main> as the button
    // so it cannot accidentally pick up a future form on the page that
    // isn't the create form for this DNS provider.
    var scope = btn.closest('main') || document;
    var form = scope.querySelector('form[data-dns-form]');
    if (!form) return;
    // Encode the form as application/x-www-form-urlencoded so the
    // server's URL-encoded CSRF/body parser can read _csrf. A raw FormData
    // would force `Content-Type: multipart/form-data; boundary=…` and
    // `query_param_opt` (which splits on `&`) would silently see no fields.
    var params = new URLSearchParams(new FormData(form));
    var resultEl = scope.querySelector('[data-test-target]');
    if (resultEl) {
      resultEl.replaceChildren(
        Object.assign(document.createElement('span'), {
          className: 'text-slate-500',
          textContent: 'Verifying…',
        })
      );
    }
    btn.disabled = true;
    fetch('/dns/test', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: params.toString(),
      credentials: 'same-origin',
    })
      .then(function (r) {
        // Preserve HTTP status so we can surface it in the error banner
        // when the server's JSON envelope doesn't include one. A failing
        // .json() (e.g. HTML error page, empty body) must not throw — fall
        // back to an empty body and let the HTTP status do the talking.
        return r
          .json()
          .then(function (j) {
            return { ok: r.ok, status: r.status, body: j };
          })
          .catch(function () {
            return { ok: r.ok, status: r.status, body: {} };
          });
      })
      .then(function (resp) {
        var body = resp.body || {};
        // Backfill `status` from the HTTP code when the server didn't echo
        // it. The body itself never gets rendered as HTML, only its text
        // fields via setTestResult's textContent path.
        var httpStatus = body.status != null ? body.status : resp.status;
        if (resp.ok && body.ok) {
          setTestResult(resultEl, 'ok', 'Credentials verified');
        } else {
          var msg = body.error || ('HTTP ' + (httpStatus != null ? httpStatus : 'error'));
          setTestResult(resultEl, 'err', msg);
        }
      })
      .catch(function (err) {
        // Network failure (DNS resolution, abort, CORS, etc.). Never inject
        // err.message into HTML — textContent via setTestResult handles it.
        setTestResult(resultEl, 'err', err && err.message ? err.message : 'request failed');
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

  // ── Site form backend mode toggle ─────────────────────────────────────
  function initBackendForm() {
    var form = document.querySelector('[data-backend-form]');
    if (!form) return;

    var directFields = document.getElementById('backend-direct-fields');
    var tunnelFields = document.getElementById('backend-tunnel-fields');
    var hidden = document.getElementById('site-backend-hidden');
    var directProto = document.getElementById('site-backend-direct-protocol');
    var directInput = document.getElementById('site-backend-direct');
    var tunnelSelect = document.getElementById('site-backend-tunnel');
    var protocolSelect = document.getElementById('site-backend-protocol');
    var hostInput = document.getElementById('site-backend-host');
    var preview = document.getElementById('backend-preview');

    function assembleUrl(scheme, host) {
      if (scheme === 'file') {
        var path = host.replace(/^\/+/, '');
        return 'file:///' + path;
      }
      return scheme + '://' + host;
    }

    function updateHidden() {
      var modeRadio = document.querySelector('input[name="route_mode"]:checked');
      if (!modeRadio || !hidden) return;
      var modeValue = modeRadio.value;
      var value = '';
      if (modeValue === 'direct') {
        var proto = directProto ? directProto.value : 'http';
        var host = directInput ? directInput.value.trim() : '';
        value = assembleUrl(proto, host);
      } else {
        var tun = tunnelSelect ? tunnelSelect.value : '';
        var proto2 = protocolSelect ? protocolSelect.value : 'http';
        var host2 = hostInput ? hostInput.value.trim() : '';
        value = tun + ':' + assembleUrl(proto2, host2);
        if (preview) preview.textContent = value || '(empty)';
      }
      hidden.value = value;
    }

    function toggleBackendMode() {
      var modeRadio = document.querySelector('input[name="route_mode"]:checked');
      if (!modeRadio || !directFields || !tunnelFields) return;
      var mode = modeRadio.value;
      if (mode === 'direct') {
        directFields.classList.remove('hidden');
        tunnelFields.classList.add('hidden');
      } else {
        directFields.classList.add('hidden');
        tunnelFields.classList.remove('hidden');
      }
      updateHidden();
    }

    function toggleHostCustom() {
      var mode = document.getElementById('host-mode');
      var wrapper = document.getElementById('host-custom-wrapper');
      if (!mode || !wrapper) return;
      if (mode.value === 'custom') {
        wrapper.classList.remove('hidden');
      } else {
        wrapper.classList.add('hidden');
      }
    }

    function showBackendError(msg) {
      var summary = document.getElementById('form-error-summary');
      if (!summary) {
        summary = document.createElement('div');
        summary.id = 'form-error-summary';
        summary.setAttribute('role', 'alert');
        summary.className = 'flex items-start gap-3 p-3.5 bg-red-50 dark:bg-red-900/30 border border-red-200 dark:border-red-500/30 rounded-lg';
        summary.innerHTML = '<svg class="w-5 h-5 flex-shrink-0 text-red-500 dark:text-red-400 mt-0.5" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M12 9v3.75m-9.303 3.376c-.866 1.5.217 3.374 1.948 3.374h14.71c1.73 0 2.813-1.874 1.948-3.374L13.949 3.378c-.866-1.5-3.032-1.5-3.898 0L2.697 16.126zM12 15.75h.007v.008H12v-.008z"/></svg><div class="flex-1 min-w-0"><p class="text-sm font-medium text-red-800 dark:text-red-200"></p></div>';
        form.insertBefore(summary, form.firstChild);
      }
      summary.querySelector('p').textContent = msg;
      if (directInput) directInput.classList.add('border-red-500', 'border-2', 'bg-red-50', 'dark:bg-red-900/20');
      if (hostInput) hostInput.classList.add('border-red-500', 'border-2', 'bg-red-50', 'dark:bg-red-900/20');
    }

    // Event listeners
    document.addEventListener('change', function (e) {
      if (e.target.matches('input[name="route_mode"]')) {
        toggleBackendMode();
      }
      if (e.target.id === 'host-mode') {
        toggleHostCustom();
      }
      if (e.target === directProto || e.target === protocolSelect || e.target === tunnelSelect) {
        updateHidden();
      }
    });

    document.addEventListener('input', function (e) {
      if (e.target === directInput || e.target === hostInput) {
        updateHidden();
      }
    });

    form.addEventListener('submit', function (e) {
      updateHidden();
      if (!hidden.value || hidden.value === '://' || hidden.value.endsWith('://')) {
        e.preventDefault();
        showBackendError('Backend is required — fill in the host:port (or file path) field');
        if (directInput && !directFields.classList.contains('hidden') && !directInput.value.trim()) {
          directInput.focus();
        } else if (hostInput && !hostInput.value.trim()) {
          hostInput.focus();
        }
      }
    });

    // Initialize
    toggleBackendMode();
    toggleHostCustom();
    updateHidden();
  }

  // Initialize backend form if present
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initBackendForm);
  } else {
    initBackendForm();
  }
})();