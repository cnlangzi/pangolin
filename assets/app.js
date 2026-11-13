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
//   - client-IP lookup (data-asn-trigger → data-asn-ip cell)
//   - IP lookup menu (data-ip-menu-toggle → data-ip-menu-panel)
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

  // ── Client-IP lookup (issue #134) ──────────────────────────────────────
  //
  // The magnifier button next to a client IP asks a third-party service
  // for the geo and the ASN of that address and renders the answer
  // under it. The lookup only runs when the operator clicks — nothing
  // is requested on page load.
  //
  // The markup (the button, the magnifier, the spinner and the two
  // result lines) is rendered by the templates: the Tailwind content
  // glob does not scan this file, so a class that only appears here
  // is purged from the stylesheet and the element ships unstyled. The
  // code below only writes text into the placeholders and toggles
  // their visibility.

  var ASN_CACHE_KEY = 'pangolin-asn-cache-v1';
  // A successful answer is reused for a day.
  var ASN_CACHE_TTL_MS = 24 * 60 * 60 * 1000;
  // A failure is remembered for five minutes. The entry is *not* a
  // block: the button promises "click to retry", so a click always
  // looks the address up again. The window only decides how long the
  // failed state is remembered — after it, the button is back to its
  // default self.
  var ASN_FAIL_TTL_MS = 5 * 60 * 1000;
  // Every provider gets its own budget: a hanging provider must not
  // eat the next one's.
  var ASN_TIMEOUT_MS = 6000;

  var ASN_PROVIDERS = [
    {
      name: 'ipwhois.app',
      url: function (ip) {
        return 'https://ipwhois.app/json/' + ipPath(ip);
      },
      normalize: normalizeIpwhois,
    },
    {
      name: 'ipinfo.io',
      url: function (ip) {
        return 'https://ipinfo.io/' + ipPath(ip) + '/json';
      },
      normalize: normalizeIpinfo,
    },
  ];

  // The address goes into the path of the URL. A colon is legal in a
  // path segment — and not every provider decodes a percent-encoded
  // IPv6 address — so it is restored after the encoding.
  function ipPath(ip) {
    return encodeURIComponent(ip).replace(/%3A/gi, ':');
  }

  function asnText(value) {
    var s = value == null ? '' : String(value).trim();
    if (!s) return '';
    // The ASN is shown as "AS15169" — the provider may leave the "AS"
    // prefix out. A value that is neither a number nor an "AS<number>"
    // is not an ASN at all, and must not be turned into one.
    if (/^as\d+$/i.test(s)) return s.toUpperCase();
    return /^\d+$/.test(s) ? 'AS' + s : '';
  }

  // ipwhois.app answers { success, asn, org, city, region, country,
  // country_code } — `asn` is a bare number and `country` is the
  // country *name*.
  function normalizeIpwhois(body) {
    if (!body || body.success === false) return null;
    return {
      asn: asnText(body.asn),
      org: body.org == null ? '' : String(body.org).trim(),
      city: body.city == null ? '' : String(body.city).trim(),
      region: body.region == null ? '' : String(body.region).trim(),
      countryName: body.country == null ? '' : String(body.country).trim(),
      countryCode:
        body.country_code == null ? '' : String(body.country_code).trim(),
    };
  }

  // ipinfo.io answers { city, region, country, org } — `country` is the
  // country *code* and `org` is "AS15169 Google LLC".
  function normalizeIpinfo(body) {
    if (!body) return null;
    var org = body.org == null ? '' : String(body.org).trim();
    var asn = '';
    var name = '';
    var cut = org.indexOf(' ');
    // The organisation is split on its first space — but only when
    // the first token really is an ASN. An organisation without one
    // ("Google LLC") must not be read as the ASN "ASGoogle".
    if (cut > 0 && /^as\d+/i.test(org.slice(0, cut))) {
      asn = asnText(org.slice(0, cut));
      name = org.slice(cut + 1).trim();
    } else if (/^as\d+/i.test(org)) {
      asn = asnText(org);
    } else {
      name = org;
    }
    return {
      asn: asn,
      org: name,
      city: body.city == null ? '' : String(body.city).trim(),
      region: body.region == null ? '' : String(body.region).trim(),
      countryName: '',
      countryCode: body.country == null ? '' : String(body.country).trim(),
    };
  }

  // An answer without a single field carries nothing — the provider
  // declined (a bogon address, a private range).
  function isUsable(data) {
    return !!(
      data.asn ||
      data.org ||
      data.city ||
      data.region ||
      data.countryName ||
      data.countryCode
    );
  }

  function fetchJson(url) {
    var controller =
      typeof AbortController === 'function' ? new AbortController() : null;
    var timer = controller
      ? window.setTimeout(function () {
          controller.abort();
        }, ASN_TIMEOUT_MS)
      : null;
    return fetch(url, controller ? { signal: controller.signal } : undefined)
      .then(function (resp) {
        if (!resp.ok) throw new Error('HTTP ' + resp.status);
        return resp.json();
      })
      .finally(function () {
        if (timer) window.clearTimeout(timer);
      });
  }

  // The cache is read and written defensively: a browser that denies
  // localStorage access still gets a working lookup, just without
  // caching.
  function readCache() {
    try {
      var raw = window.localStorage.getItem(ASN_CACHE_KEY);
      if (!raw) return {};
      var parsed = JSON.parse(raw);
      return parsed && typeof parsed === 'object' ? parsed : {};
    } catch (e) {
      return {};
    }
  }

  function writeCache(cache) {
    try {
      window.localStorage.setItem(ASN_CACHE_KEY, JSON.stringify(cache));
    } catch (e) {
      /* the cache is an optimisation, not a requirement */
    }
  }

  // The cached entry of an address, or null when there is none or the
  // one that is there has expired.
  function cacheEntry(ip) {
    return liveEntry(readCache()[ip]);
  }

  // The same check against an entry that has already been read out of
  // the cache — so a pass over many rows parses the cache only once.
  function liveEntry(entry) {
    if (!entry || typeof entry !== 'object') return null;
    var age = Date.now() - (entry.ts || 0);
    var ttl = entry.failed ? ASN_FAIL_TTL_MS : ASN_CACHE_TTL_MS;
    return age < ttl ? entry : null;
  }

  // The providers are tried in order; the first usable answer wins.
  function lookupAsn(ip) {
    var providers = ASN_PROVIDERS.slice();
    function attempt() {
      if (!providers.length) {
        return Promise.reject(new Error('no provider answered'));
      }
      var provider = providers.shift();
      return fetchJson(provider.url(ip))
        .then(function (body) {
          var data = provider.normalize(body);
          if (!data || !isUsable(data)) {
            throw new Error('no usable answer');
          }
          data.provider = provider.name;
          data.ts = Date.now();
          return data;
        })
        .catch(function () {
          // This provider declined or hung — the next one gets a turn.
          return attempt();
        });
    }
    return attempt();
  }

  // Fill the two result lines and hide the button.
  function renderAsnResult(cell, data) {
    var geo = [data.city, data.region, data.countryName || data.countryCode]
      .filter(Boolean)
      .join(', ');
    var asn = [data.asn, data.org].filter(Boolean).join(' ');
    var geoEl = cell.querySelector('[data-asn-geo]');
    var asnEl = cell.querySelector('[data-asn-asn]');
    if (geoEl) {
      geoEl.textContent = geo;
      geoEl.classList.toggle('hidden', !geo);
    }
    if (asnEl) {
      asnEl.textContent = asn;
      asnEl.classList.toggle('hidden', !asn);
      // The answer comes from a third party — the operator can see
      // which one answered.
      asnEl.title = data.provider ? 'answered by ' + data.provider : '';
    }
    var btn = cell.querySelector('[data-asn-trigger]');
    if (btn) btn.classList.add('hidden');
  }

  // Swap the magnifier for the spinner while the lookup runs.
  function setAsnBusy(cell, busy) {
    var btn = cell.querySelector('[data-asn-trigger]');
    if (!btn) return;
    btn.disabled = busy;
    var icon = btn.querySelector('[data-asn-icon]');
    var spinner = btn.querySelector('[data-asn-spinner]');
    if (icon) icon.classList.toggle('hidden', busy);
    if (spinner) spinner.classList.toggle('hidden', !busy);
  }

  document.addEventListener('click', function (e) {
    var btn = e.target && e.target.closest ? e.target.closest('[data-asn-trigger]') : null;
    if (!btn) return;
    e.preventDefault();
    if (btn.disabled) return;
    var cell = btn.closest('[data-asn-ip]');
    if (!cell) return;
    var ip = cell.dataset.asnIp;
    if (!ip) return;

    // A cached answer is rendered right away — no network request.
    var cached = cacheEntry(ip);
    if (cached && !cached.failed) {
      renderAsnResult(cell, cached);
      return;
    }

    setAsnBusy(cell, true);
    lookupAsn(ip)
      .then(function (data) {
        var cache = readCache();
        cache[ip] = data;
        writeCache(cache);
        renderAsnResult(cell, data);
      })
      .catch(function () {
        // Every provider failed: no result lines are rendered, the
        // button stays and the operator can try again.
        var cache = readCache();
        cache[ip] = { failed: true, ts: Date.now() };
        writeCache(cache);
        btn.title = 'lookup failed — click to retry';
      })
      .then(function () {
        setAsnBusy(cell, false);
      });
  });

  // Both tables live in regions that htmx replaces wholesale every two
  // seconds, so the lines written into a cell are discarded on the
  // next refresh. Re-render the cached results after every swap — and
  // on the first paint — so the answer survives the refresh without
  // another lookup.
  function rehydrateAsnCells() {
    var cells = document.querySelectorAll('[data-asn-ip]');
    if (!cells.length) return;
    // The cache is read once for the whole pass — every row looks at
    // the same map, and the table is re-rendered every two seconds.
    var cache = readCache();
    for (var i = 0; i < cells.length; i++) {
      var cell = cells[i];
      var ip = cell.dataset.asnIp;
      if (!ip) continue;
      var cached = liveEntry(cache[ip]);
      // A cached failure leaves the button in place — the operator can
      // click it again.
      if (!cached || cached.failed) continue;
      renderAsnResult(cell, cached);
    }
  }

  document.addEventListener('htmx:afterSwap', function () {
    rehydrateAsnCells();
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', rehydrateAsnCells);
  } else {
    rehydrateAsnCells();
  }

  // ── IP lookup menu (issue #149) ─────────────────────────────────────────
  //
  // The caret next to a client IP opens a menu of IP-intelligence
  // sites: the operator picks the site that answers the question
  // they actually have (scanner? abuse report? proxy? port?). The
  // address itself stays plain text — the menu offers several
  // destinations, so the address cannot be one of them.
  //
  // The panel is a **single** page-level element, rendered by the
  // layout outside every region htmx replaces: the tables refresh
  // every two seconds, and a per-row menu would destroy itself in
  // the middle of the interaction.
  //
  // The markup (the panel, the entries and their URL templates)
  // lives in a template, not here: the Tailwind content glob does
  // not scan this file, so a class that only appears here is purged
  // from the stylesheet and the menu ships unstyled. The code below
  // only fills in the `href` of the entries, positions the panel
  // and toggles its visibility.

  // The caret that opened the menu, and the address it was opened
  // for.
  //
  // The caret can go **stale**: the tables refresh every two seconds
  // and the row that carried it is replaced, so the node this points
  // at is no longer in the document. The address is what stays
  // true — the menu keeps showing the address of the row that
  // opened it.
  var ipMenuButton = null;
  var ipMenuIp = null;

  function ipMenuPanel() {
    return document.querySelector('[data-ip-menu-panel]');
  }

  // Whether the node is still in the document.
  function isConnected(node) {
    if (!node) return false;
    if (typeof node.isConnected !== 'boolean') return true;
    return node.isConnected;
  }

  function closeIpMenu() {
    var panel = ipMenuPanel();
    if (panel) {
      panel.classList.add('hidden');
      panel.removeAttribute('data-ip');
    }
    if (ipMenuButton) {
      ipMenuButton.setAttribute('aria-expanded', 'false');
      ipMenuButton = null;
    }
    ipMenuIp = null;
  }

  function openIpMenu(button, ip) {
    var panel = ipMenuPanel();
    if (!panel) return;
    // Only one menu is open: the caret that opened the previous
    // one is marked as closed again.
    if (ipMenuButton && ipMenuButton !== button) {
      ipMenuButton.setAttribute('aria-expanded', 'false');
    }
    // Every entry points at its own site, for this address. The
    // address is rendered by the server from an `IpAddr`, and
    // `ipPath` keeps the colons of an IPv6 address — but the
    // encoding is done anyway, so a stray character can never break
    // the URL.
    var entries = panel.querySelectorAll('[data-ip-site]');
    for (var i = 0; i < entries.length; i++) {
      var entry = entries[i];
      var template = entry.getAttribute('data-url-template') || '';
      entry.setAttribute('href', template.replace('{ip}', ipPath(ip)));
    }
    panel.setAttribute('data-ip', ip);
    // The panel sits under the caret: absolute coordinates taken
    // from the caret's viewport rectangle plus the page scroll
    // offset. A `fixed` panel would stay behind when the page
    // scrolls, and the table clips its own overflow.
    var rect = button.getBoundingClientRect();
    var scrollX = window.scrollX || 0;
    var scrollY = window.scrollY || 0;
    var left = rect.left + scrollX;
    var top = rect.bottom + scrollY;
    // The panel has to be visible before it can be measured.
    panel.classList.remove('hidden');
    // Keep the panel on the screen: the tables scroll sideways, so
    // a caret can sit near the right edge.
    var width = panel.offsetWidth || 0;
    var viewport = window.innerWidth || 0;
    if (width > 0 && viewport > 0) {
      var maxLeft = viewport + scrollX - width - 8;
      if (left > maxLeft) left = Math.max(scrollX + 8, maxLeft);
    }
    panel.style.left = Math.round(left) + 'px';
    panel.style.top = Math.round(top) + 'px';
    ipMenuButton = button;
    ipMenuIp = ip;
    button.setAttribute('aria-expanded', 'true');
  }

  document.addEventListener('click', function (e) {
    var target = e.target;
    // An entry opens its site in a new tab — the menu has done its
    // job, so it goes away.
    if (target && target.closest && target.closest('[data-ip-site]')) {
      closeIpMenu();
      return;
    }
    var toggle =
      target && target.closest ? target.closest('[data-ip-menu-toggle]') : null;
    if (toggle) {
      var ip = toggle.getAttribute('data-ip');
      if (!ip) return;
      // The same caret closes the menu it opened. After a refresh
      // the caret is a *new* node — the menu is still open for that
      // address, so the address decides then.
      var sameCaret = ipMenuButton === toggle;
      var staleCaret = ipMenuButton !== null && !isConnected(ipMenuButton);
      if (ipMenuIp === ip && (sameCaret || staleCaret)) {
        closeIpMenu();
        return;
      }
      openIpMenu(toggle, ip);
      return;
    }
    // A click outside the menu closes it.
    if (!ipMenuButton) return;
    var panel = ipMenuPanel();
    var inPanel =
      panel && target && target.closest
        ? target.closest('[data-ip-menu-panel]')
        : null;
    if (!inPanel) closeIpMenu();
  });

  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' || e.key === 'Esc') closeIpMenu();
  });
})();