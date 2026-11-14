// Behaviour of the landing pages: the install steps, the copy button, the phone demo and
// the star count. Shared by every language page; the text people read comes from a
// dictionary chosen by <html lang>.
//
// A language page sets its dictionary before loading this script:
//   <script>window.TGMCP_STRINGS = { ru: { copy: 'Копировать', ... } };</script>
//   <script src="/landing.js"></script>
// Any key it leaves out, or gives with the wrong shape, falls back to English (EN below).
// Lists may have another length, except a case's `chips` (one per tool call). `stars` is
// read as the page gives it, since languages have different plural forms.
// Text outside the script (headline, definition, FAQ, demo replies) is markup on each page.
(function () {
// The English dictionary, and the fallback for every other language.
var EN = {
  copy: 'Copy',
  copied: 'Copied',
  // The star count's aria-label. {n} is the count; keys are Intl.PluralRules categories of
  // the page's language, `other` is used for a category the page leaves out.
  stars: { one: 'GitHub, {n} star', other: 'GitHub, {n} stars' },
  // The star count from 1000 on; {n} is the count in thousands, formatted for the language.
  thousands: '{n}k',
  // {case} is the playing scenario's description.
  phoneLabel: 'Demo in the Claude app on a phone: {case}',
  apps: {
    chatgpt: {
      note: 'Paid ChatGPT plans.',
      steps: ['Open Settings → Apps & Connectors → Advanced and turn on Developer mode.',
        'Click Create, name it Telegram and paste the link below.',
        'Sign in by scanning the QR code with Telegram on your phone.']
    },
    claude: {
      note: 'Works on the free plan too.',
      steps: ['Open Settings → Connectors and click Add custom connector.',
        'Name it Telegram and paste the link below.',
        'Click Connect and scan the QR code with Telegram on your phone.']
    },
    gemini: {
      note: 'Personal Google accounts. Add it on the web, use it on your phone.',
      steps: ['On gemini.google.com, open Settings → Connected apps.',
        'Choose Add a custom app and paste the link below.',
        'Approve, then scan the QR code with Telegram on your phone.']
    }
  },
  // Phone demo: what the user types, Claude's first line, one detail per tool call (in the
  // order of CASES[].chips) and the description read out by screen readers.
  cases: {
    sell: {
      msg: 'I want to sell a few things: my bike, an armchair and a monitor. Make a Telegram channel for the sale',
      intro: 'On it. Setting up your channel.',
      chips: ['Weekend garage sale', 'bike, armchair, monitor', 't.me/weekend_garage_sale'],
      label: 'someone asks to sell a bike, an armchair and a monitor; Claude creates a Telegram channel, posts each item with its photo and returns an invite link.'
    },
    catch: {
      msg: 'What did I miss since yesterday evening? Anything I need to answer?',
      intro: 'Let me go through your unread chats.',
      chips: ['unread only → 6 chats, 94 messages', 'since yesterday 18:00 → read 94 of 94'],
      label: 'someone asks what they missed; Claude reads six unread chats and lists the two that need a reply.'
    },
    table: {
      msg: 'Book a table for 4 at Osteria Lume on Friday, around 8 pm',
      intro: 'Sure, I\'ll message them.',
      chips: ['Osteria Lume', 'a table for 4, Friday around 20:00?', 'reply: 20:30 on the terrace', '20:30 on the terrace works'],
      label: 'someone asks to book a table for four at Osteria Lume on Friday; Claude messages the restaurant in Telegram, reads the offer of 20:30 on the terrace and confirms it.'
    }
  }
};

// `over` laid on `base`, key by key. Only keys of `base` count; a value of another type, or
// a list that is empty or holds a value of another type, is ignored, so a broken or partial
// dictionary still yields a complete one.
function mergeStrings(base, over) {
  if (Array.isArray(base)) {
    var ok = Array.isArray(over) && over.length > 0 &&
      over.every(function (o) { return typeof o === typeof base[0]; });
    return ok ? over : base;
  }
  if (base && typeof base === 'object') {
    var out = {};
    var ok = over && typeof over === 'object' && !Array.isArray(over);
    Object.keys(base).forEach(function (k) { out[k] = mergeStrings(base[k], ok ? over[k] : undefined); });
    return out;
  }
  return typeof over === typeof base ? over : base;
}

  var lang = (document.documentElement.getAttribute('lang') || 'en').toLowerCase().split('-')[0];
  var dicts = window.TGMCP_STRINGS || {};
  var own = Object.prototype.hasOwnProperty.call(dicts, lang) ? dicts[lang] : undefined;
  var S = mergeStrings(EN, own);
  function fill(t, vars) {
    return t.replace(/\{(\w+)\}/g, function (m, k) { return k in vars ? vars[k] : m; });
  }

  var MCP_URL = 'https://connector.tgmcp.dev/mcp';
  // The order matches the buttons' data-app index.
  var APPS = [
    { key: 'chatgpt', name: 'ChatGPT', url: 'https://chatgpt.com' },
    { key: 'claude', name: 'Claude', url: 'https://claude.ai' },
    { key: 'gemini', name: 'Gemini', url: 'https://gemini.google.com' }
  ];
  // Phone demo. Phases: 0 typing, 1 sent, one per tool call, the reply, then (with a Telegram
  // screen) the screen opening and one phase per item appearing on it.
  var CASES = [
    { key: 'sell', attach: true, tg: 'sell', items: 3, replyMs: 2800,
      chips: [{ name: 'create_channel', ms: 1300 }, { name: 'send_photo ×3', ms: 1700 },
        { name: 'export_invite_link', ms: 1000 }] },
    { key: 'catch', attach: false, tg: '', items: 0, replyMs: 6500,
      chips: [{ name: 'list_chats', ms: 1300 }, { name: 'get_messages ×6', ms: 2000 }] },
    { key: 'table', attach: false, tg: 'table', items: 4, replyMs: 2600,
      chips: [{ name: 'find_chats', ms: 1100 }, { name: 'send_message', ms: 1300 },
        { name: 'get_messages', ms: 1900 }, { name: 'send_message', ms: 1100 }] }
  ];
  // A case's chip details pair one-to-one with its tool calls: another count is English.
  CASES.forEach(function (pc) {
    if (S.cases[pc.key].chips.length !== pc.chips.length) S.cases[pc.key].chips = EN.cases[pc.key].chips;
  });
  function wordsOf(pc) { return S.cases[pc.key]; }
  function phaseMs(pc) {
    var ms = [700, 900];
    pc.chips.forEach(function (c) { ms.push(c.ms); });
    ms.push(pc.replyMs);
    if (pc.tg) {
      ms.push(700);
      for (var i = 1; i <= pc.items; i++) ms.push(i === pc.items ? 4200 : 900);
    }
    return ms;
  }

  var ACCENT = '#E4B2EA';
  var SPIN = '<svg class="spin" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#A8A49A" stroke-width="2.5" stroke-linecap="round" aria-hidden="true" style="flex: none"><path d="M21 12a9 9 0 1 1-6.2-8.6"></path></svg>';
  var TICK = '<svg class="tick" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="#6FD39A" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" style="flex: none"><path d="m5 12 5 5 9-10"></path></svg>';

  var state = { ps: 0, ph: 0, chars: 0, app: -1, copied: false };
  var phone = document.getElementById('phone');
  var reduce = false;
  try { reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches; } catch (e) {}
  var auto = !reduce;
  var visible = true;
  var builtCase = -1;
  var pt, ti, ct;

  function all(sel) { return Array.prototype.slice.call(document.querySelectorAll(sel)); }
  function esc(t) {
    return String(t).replace(/[&<>"]/g, function (c) { return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]; });
  }
  function chip(c, i, details) {
    return '<div data-if="c' + i + 'show" hidden style="display: contents">' +
      '<div class="bub" style="display: flex; align-items: center; gap: 10px; background: #2A2926; border: 1px solid #3A3934; border-radius: 12px; padding: 8px 12px">' +
      '<div data-if="c' + i + 'run" hidden style="display: contents">' + SPIN + '</div>' +
      '<div data-if="c' + i + 'done" hidden style="display: contents">' + TICK + '</div>' +
      '<span style="min-width: 0; display: flex; flex-direction: column">' +
      '<span class="mono" style="font-size: 12px; color: #D9D5CC">telegram · ' + esc(c.name) + '</span>' +
      '<span style="font-size: 12px; color: #A8A49A; white-space: nowrap; overflow: hidden; text-overflow: ellipsis">' + esc(details[i]) + '</span>' +
      '</span></div></div>';
  }

  function render() {
    var pc = CASES[state.ps];
    var words = wordsOf(pc);
    var ph = state.ph;
    var R = 2 + pc.chips.length;
    var T = R + 1;
    var typing = ph === 0;
    var onTg = !!pc.tg && ph >= T;
    var reply = ph >= R;

    if (builtCase !== state.ps) {
      all('[data-chips]').forEach(function (box) {
        box.innerHTML = pc.chips.map(function (c, i) { return chip(c, i, words.chips); }).join('');
      });
      builtCase = state.ps;
    }

    var v = {
      typing: typing, idle: !typing, attach: pc.attach,
      user: ph >= 1, a1: ph >= 2,
      rSell: reply && pc.key === 'sell', rCatch: reply && pc.key === 'catch', rTable: reply && pc.key === 'table',
      chat: !onTg, tgSell: onTg && pc.tg === 'sell', tgTable: onTg && pc.tg === 'table',
      it1: ph >= T + 1, it2: ph >= T + 2, it3: ph >= T + 3, it4: ph >= T + 4,
      cap0: state.ps === 0, cap1: state.ps === 1, cap2: state.ps === 2,
      appOpen: state.app >= 0
    };
    pc.chips.forEach(function (c, i) {
      v['c' + i + 'show'] = ph >= 2 + i;
      v['c' + i + 'run'] = ph === 2 + i;
      v['c' + i + 'done'] = ph > 2 + i;
    });
    // Only flip what changed: an element leaving display:none replays its entrance.
    all('[data-if]').forEach(function (el) {
      var on = !!v[el.getAttribute('data-if')];
      if (el.hidden === on) el.hidden = !on;
    });

    var cur = APPS[state.app] || APPS[0];
    var curText = S.apps[cur.key];
    var shown = {
      msg: words.msg, intro: words.intro, typed: words.msg.slice(0, state.chars),
      appName: cur.name, appNote: curText.note, copyLabel: state.copied ? S.copied : S.copy
    };
    all('[data-text]').forEach(function (el) {
      var t = shown[el.getAttribute('data-text')];
      if (el.textContent !== t) el.textContent = t;
    });
    all('[data-send]').forEach(function (el) {
      el.style.background = typing && state.chars > 0 ? ACCENT : '#4A4842';
    });
    if (phone) phone.setAttribute('aria-label', fill(S.phoneLabel, { 'case': words.label }));

    all('[data-app]').forEach(function (btn) {
      var on = +btn.getAttribute('data-app') === state.app;
      btn.setAttribute('aria-expanded', on ? 'true' : 'false');
      btn.style.background = on ? '#F1ECF7' : 'rgba(30,25,40,.6)';
      btn.style.color = on ? '#000' : '#F1ECF7';
      btn.style.borderColor = on ? '#F1ECF7' : 'rgba(241,236,247,.14)';
    });
    all('[data-app-url]').forEach(function (a) { a.href = cur.url; a.setAttribute('data-umami-event-app', cur.name); });
    var steps = document.getElementById('app-steps');
    if (steps && steps.getAttribute('data-for') !== cur.key) {
      steps.setAttribute('data-for', cur.key);
      steps.innerHTML = curText.steps.map(function (t, i) {
        return '<li style="display: flex; gap: 12px; align-items: baseline"><span class="mono" style="flex: none; width: 22px; height: 22px; border-radius: 50%; background: #2D2738; color: #F1ECF7; font-size: 12px; display: inline-flex; align-items: center; justify-content: center">' + (i + 1) + '</span><span style="color: #D9D0E6">' + esc(t) + '</span></li>';
      }).join('');
    }
  }

  function staticPhone(ps) {
    var pc = CASES[ps];
    state.ps = ps;
    state.ph = 2 + pc.chips.length;
    state.chars = wordsOf(pc).msg.length;
  }

  function schedule() {
    clearTimeout(pt);
    clearInterval(ti);
    if (!(auto && visible && !document.hidden)) return;
    var pc = CASES[state.ps];
    var ph = state.ph;
    var len = wordsOf(pc).msg.length;
    if (ph === 0 && state.chars < len) {
      ti = setInterval(function () {
        state.chars += 1;
        if (state.chars >= len) {
          state.chars = len;
          clearInterval(ti);
          render();
          schedule();
        } else {
          render();
        }
      }, 30);
      return;
    }
    var ms = phaseMs(pc);
    pt = setTimeout(function () {
      if (ph + 1 < ms.length) {
        state.ph = ph + 1;
      } else {
        state.ps = (state.ps + 1) % CASES.length;
        state.ph = 0;
        state.chars = 0;
      }
      render();
      schedule();
    }, ms[ph]);
  }

  all('[data-app]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      var i = +btn.getAttribute('data-app');
      state.app = state.app === i ? -1 : i;
      render();
    });
  });
  all('[data-copy]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      try {
        navigator.clipboard.writeText(MCP_URL).then(function () {
          state.copied = true;
          render();
          clearTimeout(ct);
          ct = setTimeout(function () { state.copied = false; render(); }, 1600);
        }, function () {});
      } catch (e) {}
    });
  });

  if (!auto) staticPhone(0);
  try {
    if (phone && 'IntersectionObserver' in window) {
      new IntersectionObserver(function (entries) {
        visible = entries[0].isIntersecting;
        schedule();
      }).observe(phone);
    }
  } catch (e) {}
  document.addEventListener('visibilitychange', schedule);
  render();
  schedule();

  // Stars on the repository, next to the GitHub link. GitHub's public API
  // (60 requests an hour per visitor), cached for an hour; nothing shown if it fails.
  (function () {
    var box = document.querySelector('[data-stars]');
    var out = document.querySelector('[data-stars-count]');
    var link = document.querySelector('[data-repo-link]');
    if (!box || !out) return;
    var KEY = 'tgmcp-stars';
    function plural(l, n) {
      try { return new Intl.PluralRules(l).select(n); } catch (e) { return n === 1 ? 'one' : 'other'; }
    }
    // Plural forms vary by language (Russian has one/few/many), so the page's own `stars`
    // is read directly rather than merged onto English's one/other.
    function label(n) {
      var forms = own && own.stars;
      if (forms && typeof forms === 'object') {
        var f = forms[plural(lang, n)];
        if (typeof f !== 'string') f = forms.other;
        if (typeof f === 'string') return fill(f, { n: n });
      }
      return fill(EN.stars[plural('en', n)] || EN.stars.other, { n: n });
    }
    function thousands(n) {
      var digits = n >= 10000 ? 0 : 1;
      try { return new Intl.NumberFormat(lang, { maximumFractionDigits: digits }).format(n / 1000); } catch (e) {}
      return (n / 1000).toFixed(digits).replace(/\.0$/, '');
    }
    function show(n) {
      if (typeof n !== 'number' || n < 0) return;
      out.textContent = n >= 1000 ? fill(S.thousands, { n: thousands(n) }) : String(n);
      box.hidden = false;
      if (link) link.setAttribute('aria-label', label(n));
    }
    try {
      var cached = JSON.parse(localStorage.getItem(KEY) || 'null');
      if (cached && Date.now() - cached.at < 3600 * 1000) return show(cached.n);
    } catch (e) {}
    fetch('https://api.github.com/repos/qelphybox/tg-mcp', { headers: { Accept: 'application/vnd.github+json' } })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (repo) {
        if (!repo || typeof repo.stargazers_count !== 'number') return;
        show(repo.stargazers_count);
        try { localStorage.setItem(KEY, JSON.stringify({ n: repo.stargazers_count, at: Date.now() })); } catch (e) {}
      })
      .catch(function () {});
  })();
})();
