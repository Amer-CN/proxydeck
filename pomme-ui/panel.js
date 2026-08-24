/* ============================================================
   pomme-ui/panel.js — PommeToys 机械风设置面板（复刻版逻辑）
   行为规格：.work/spec/interaction-notes.md（唯一真源）
   file:// 适配：普通 script、无 fetch、音效走 new Audio()。
   ============================================================ */
(function () {
  'use strict';

  /* ———— 工具 ———— */
  function $(sel, root) { return (root || document).querySelector(sel); }
  function $$(sel, root) { return Array.prototype.slice.call((root || document).querySelectorAll(sel)); }
  var REDUCED = matchMedia('(prefers-reduced-motion: reduce)').matches;
  var CJK_RE = /[\u2E80-\u2EFF\u3000-\u303F\u31C0-\u31EF\u3200-\u32FF\u3400-\u4DBF\u4E00-\u9FFF\uF900-\uFAFF]/;

  /* ══════════════════ 0. i18n（file:// 内联字典，无 fetch）══════════════════ */
  var I18N = (typeof PT_I18N !== 'undefined') ? PT_I18N : {};
  var LANG = 'zh';          /* 默认中文；'en' 切英文 */
  function t(key) {
    var e = I18N[key];
    if (!e) return key;
    return e[LANG] || e.zh || e.en || key;
  }
  function applyI18n() {
    $$('[data-i18n]').forEach(function (el) {
      el.textContent = t(el.getAttribute('data-i18n'));
    });
    $$('[data-i18n-attr]').forEach(function (el) {
      var spec = el.getAttribute('data-i18n-attr');
      spec.split(';').forEach(function (pair) {
        var m = pair.match(/^\s*([a-zA-Z-]+):(.+?)\s*$/);
        if (m) el.setAttribute(m[1], t(m[2]));
      });
    });
    $$('[data-i18n-ph]').forEach(function (el) {
      el.setAttribute('placeholder', t(el.getAttribute('data-i18n-ph')));
    });
    document.documentElement.setAttribute('lang', LANG === 'zh' ? 'zh-CN' : 'en');
    var lb = $('#ptLangBtn span');
    if (lb) lb.textContent = LANG === 'zh' ? 'EN' : '中文';
  }

  /* ══════════════════ 1. 音效（interaction-notes §0/§1）══════════════════
     file:// 下 fetch 不可用 → 每个文件一个 Audio 池，快速连播时克隆节点叠放。 */
  var DECK_BASIS = 2.1826;
  var SET_FILES = {
    1: {
      modeChange: 'tick-fs626659', keyPress: 'tick-fs626659',
      switchOn: 'btn-fs690300', switchOff: 'btn-fs690300',
      gearDetent: 'syn-03-click-crisp',
      faderTick: 'syn-04-tick-micro', faderStep: 'syn-04-tick-micro',
      powerThrow: 'syn-26-bakelite-bright',
      ratchetTooth: 'syn-33-ratchet-tooth',
      activationSeat: 'syn-34-knife-seat',
      selfTestStep: 'syn-35-relay-pull',
      activationSettle: 'syn-36-bolt-home',
      activationRelease: 'syn-37-spring-release'
    },
    2: {
      modeChange: 'set2-key-press', keyPress: 'set2-key-press',
      switchOn: 'set2-switch-on', switchOff: 'set2-switch-off',
      gearDetent: 'set2-gear-detent',
      faderTick: 'set2-fader-tick', faderStep: 'set2-fader-step',
      powerThrow: 'set2-power-throw',
      ratchetTooth: 'set2-ratchet-tooth',
      activationSeat: 'set2-activation-seat',
      selfTestStep: 'set2-selftest-step',
      activationSettle: 'set2-activation-settle',
      activationRelease: 'set2-activation-release'
    }
  };
  var TRIM = {
    'tick-fs626659': 0.47, 'btn-fs690300': 0.5456, 'syn-03-click-crisp': 0.6167,
    'syn-04-tick-micro': 1.0, 'syn-26-bakelite-bright': 0.3536,
    'syn-33-ratchet-tooth': 0.5394, 'syn-34-knife-seat': 0.3153,
    'syn-35-relay-pull': 0.4581, 'syn-36-bolt-home': 0.3314, 'syn-37-spring-release': 0.2386,
    /* fax 系官方增益（Mach-O HandheldSound.Sample.gainTrim 静态逆向，.work/spec/sfx-reverse.md） */
    'fax-send-key': 0.3770, 'fax-offhook': 0.1223,
    'fax-dial-1': 0.2476, 'fax-dial-2': 0.2534, 'fax-dial-3': 0.2368,
    'fax-dial-4': 0.2606, 'fax-dial-5': 0.2522, 'fax-dial-6': 0.2367,
    'fax-carrier': 0.0881,
    'fax-feed-1': 0.3216, 'fax-feed-2': 0.3175, 'fax-feed-3': 0.2928,
    'fax-print-1': 0.7727, 'fax-print-2': 0.5556, 'fax-print-3': 0.7605,
    'fax-load-1': 0.3107, 'fax-load-2': 0.3151, 'fax-load-3': 0.3034,
    'fax-ding': 0.0997, 'fax-tear': 0.2950, 'fax-error': 0.3681
  };
  /* 全部音效文件清单（含完整 .wav 路径）。既是装配路径的来源，也让引用可 grep。 */
  var SOUND_FILES = [
    'assets/sounds/tick-fs626659.wav',
    'assets/sounds/btn-fs690300.wav',
    'assets/sounds/syn-03-click-crisp.wav',
    'assets/sounds/syn-04-tick-micro.wav',
    'assets/sounds/syn-26-bakelite-bright.wav',
    'assets/sounds/syn-33-ratchet-tooth.wav',
    'assets/sounds/syn-34-knife-seat.wav',
    'assets/sounds/syn-35-relay-pull.wav',
    'assets/sounds/syn-36-bolt-home.wav',
    'assets/sounds/syn-37-spring-release.wav',
    'assets/sounds/set2-key-press.wav',
    'assets/sounds/set2-switch-on.wav',
    'assets/sounds/set2-switch-off.wav',
    'assets/sounds/set2-gear-detent.wav',
    'assets/sounds/set2-fader-tick.wav',
    'assets/sounds/set2-fader-step.wav',
    'assets/sounds/set2-power-throw.wav',
    'assets/sounds/set2-ratchet-tooth.wav',
    'assets/sounds/set2-activation-seat.wav',
    'assets/sounds/set2-selftest-step.wav',
    'assets/sounds/set2-activation-settle.wav',
    'assets/sounds/set2-activation-release.wav',
    'assets/sounds/fax-dial-1.wav',
    'assets/sounds/fax-dial-2.wav',
    'assets/sounds/fax-dial-3.wav',
    'assets/sounds/fax-dial-4.wav',
    'assets/sounds/fax-dial-5.wav',
    'assets/sounds/fax-dial-6.wav',
    'assets/sounds/fax-ding.wav',
    'assets/sounds/fax-error.wav',
    'assets/sounds/fax-feed-1.wav',
    'assets/sounds/fax-feed-2.wav',
    'assets/sounds/fax-feed-3.wav',
    'assets/sounds/fax-load-1.wav',
    'assets/sounds/fax-load-2.wav',
    'assets/sounds/fax-load-3.wav',
    'assets/sounds/fax-offhook.wav',
    'assets/sounds/fax-carrier.wav',       /* 传真载波（官方 bundle 提取——'已接通'握手音） */
    'assets/sounds/fax-print-1.wav',
    'assets/sounds/fax-print-2.wav',
    'assets/sounds/fax-print-3.wav',
    'assets/sounds/fax-send-key.wav',
    'assets/sounds/fax-tear.wav'
  ];

  var lastSpoken = {};       /* name -> last gain+time（防连响走调用侧） */
  var gestureReady = false;  /* 首次手势前静默丢弃 */

  /* 文件名 → 清单里的完整路径（清单就是唯一来源） */
  function soundPath(name) {
    for (var i = 0; i < SOUND_FILES.length; i++) {
      if (SOUND_FILES[i].slice(0, -4) === 'assets/sounds/' + name) return SOUND_FILES[i];
    }
    return null;
  }

  /* ══════════════ 官网同款 WebAudio 音效引擎（BufferSource 版）══════════════
   链路：BufferSource → voiceGain(逐次) → master(0.50) → 软限幅 → out，与官网完全同构。
   file:// 三条路全被 Chromium 堵死：fetch/XHR 禁、MediaElementSource 源为 null 静音——
   所以 wav 以 base64 内嵌（assets/sounds-data.js），decodeAudioData 后 BufferSource 播放。
   连播叠放：每次 new BufferSource（AudioBuffer 可复用，这是官网的做法）。 */
  var MASTER_VOL = 0.50;
  var waCtx = null, waMaster = null;
  var waBufs = {};                    /* name -> AudioBuffer（解码一次） */
  var waDecoding = {};
  function waInit() {
    if (waCtx) return;
    try {
      var AC = window.AudioContext || window.webkitAudioContext;
      waCtx = new AC();
      waMaster = waCtx.createGain();
      waMaster.gain.value = MASTER_VOL;
      /* 官网软限幅（hero-scene.js LIMIT_*）：|x|≤0.92 直通，越过后 tanh 渐近 0.98 */
      var pre = waCtx.createGain(); pre.gain.value = 1 / 2.5;
      var shaper = waCtx.createWaveShaper();
      var n = 8193, curve = new Float32Array(n);
      var KNEE = 0.92, CEIL = 0.98;
      for (var i = 0; i < n; i++) {
        var x = ((i / (n - 1)) * 2 - 1) * 2.5, a = Math.abs(x);
        var yy = a <= KNEE ? a : KNEE + (CEIL - KNEE) * Math.tanh((a - KNEE) / (CEIL - KNEE));
        curve[i] = (x < 0 ? -1 : x > 0 ? 1 : 0) * yy;
      }
      shaper.curve = curve; shaper.oversample = 'none';
      waMaster.connect(pre).connect(shaper).connect(waCtx.destination);
      ['pointerdown', 'keydown', 'touchstart'].forEach(function (ev) {
        window.addEventListener(ev, function () {
          if (waCtx && waCtx.state === 'suspended') waCtx.resume();
        }, { passive: true });
      });
    } catch (e) { waCtx = null; }
  }
  waInit();
  function waBuf(name) {              /* 解码一次并缓存（懒解码） */
    if (waBufs[name]) return waBufs[name];
    if (waDecoding[name] || !waCtx || typeof SOUND_DATA === 'undefined' || !SOUND_DATA[name]) return null;
    waDecoding[name] = true;
    try {
      var bin = atob(SOUND_DATA[name]);
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      waCtx.decodeAudioData(bytes.buffer, function (buf) { waBufs[name] = buf; }, function () {});
    } catch (e) {}
    return null;                      /* 本次 null，下次（解码完成后）就有 */
  }
  /* 预解码全部音效：数据已内嵌（无网络），一次性解码 42 颗约几十 ms，
     消除"首次点击无声"（懒解码的解码空窗） */
  if (typeof SOUND_DATA !== 'undefined') {
    Object.keys(SOUND_DATA).forEach(waBuf);
  }
  function playFile(name, gain) {
    if (!gestureReady) return;                       /* 手势前不出声 */
    waBuf(name);                                     /* 触发懒解码 */
    var buf = waBufs[name];
    if (waCtx && buf) {
      if (waCtx.state === 'suspended') waCtx.resume();
      var src = waCtx.createBufferSource();
      src.buffer = buf;
      var g = waCtx.createGain();
      g.gain.value = DECK_BASIS * (TRIM[name] != null ? TRIM[name] : 1.0) * (gain == null ? 1 : gain);
      src.connect(g).connect(waMaster);
      src.start();
      if (typeof window.__sfxLog === 'function') window.__sfxLog(name);   /* 测试观测口（函数版） */
      (window.__sfxLogArr = window.__sfxLogArr || []).push(name);
      if (window.__log) window.__log.push([performance.now(), 'SFX:' + name]);   /* 音画同步观测 */
    } else if (waCtx) {
      /* 解码未完成：下次触发即有声 */
    }
  }

  /* play(slot, gain)：soundSet=0 静音；仪式增益在激活流程处叠加 */
  function play(slot, gain) {
    if (st.soundSet === 0) return 0;
    var file = SET_FILES[st.soundSet] && SET_FILES[st.soundSet][slot];
    if (!file) return 0;
    playFile(file, gain == null ? 1 : gain);
    return 1;
  }
  function playFax(name, gain) { playFile(name, gain == null ? 1 : gain); }

  /* 防连响：距上次 <30ms 丢弃；30~120ms 线性把 gain 从 0.7 补到 1.0 */
  function antiRattle(name) {
    var now = performance.now();
    if (!lastSpoken[name]) { lastSpoken[name] = now; return 1; }
    var dt = now - lastSpoken[name];
    if (dt < 30) return 0;
    lastSpoken[name] = now;
    if (dt < 120) return 0.7 + 0.3 * ((dt - 30) / 90);
    return 1;
  }

  window.addEventListener('pointerdown', function () { gestureReady = true; }, { capture: true });
  window.addEventListener('keydown', function () { gestureReady = true; }, { capture: true });

  /* ══════════════════ 2. 状态（出厂默认，§2）══════════════════ */
  var st = {
    mode: 'win',
    shake: true, dockMin: true, dockPrev: true, wless: true, cmdTab: true, hideAll: true,
    smooth: false, reverse: true,
    awake: false, awakeDisplay: false, awakeBattery: true, awakeDuration: 0,
    dockDelay: 2, dockAnim: 1, dockAppliedDelay: 2, dockAppliedAnim: 1,
    launchLogin: true, dockIcon: false, anim: true, autoUpdate: true,
    speed: 1.00, undo: 2.0,
    horiz: 'shift', precise: 'option',
    lang: 'system',
    soundSet: 1
  };
  var SHORTCUT = '⌘H';
  var MODS = ['off', 'shift', 'control', 'option', 'command'];
  var MOD_SYM = { shift: '⇧', control: '⌃', option: '⌥', command: '⌘' };
  var MOD_LABEL = { shift: 'Shift', control: 'Control', option: 'Option', command: 'Command' };
  var LANGS = ['system', 'en', 'zh', 'zh-hant', 'de', 'ja', 'fr', 'es'];
  var LANG_TITLE = {
    system: 'try.gen.langSystemTitle', en: 'English', zh: '简体中文',
    'zh-hant': '繁體中文', de: 'Deutsch', ja: '日本語', fr: 'Français', es: 'Español'
  };
  var GEAR_VALUES = {
    horiz: MODS, precise: MODS,
    lang: LANGS,
    dockDelay: [0, 1, 2, 3, 4],
    dockAnim: [0, 1, 2]
  };
  var MODES = ['win', 'scr', 'sys', 'gen'];

  /* ══════════════════ 3. 里程表 OdometerDigits（§8）══════════════════ */
  function buildOdo(el, text) {
    el.textContent = '';
    String(text).split('').forEach(function (ch) {
      if (/\d/.test(ch)) {
        var cell = document.createElement('span');
        cell.className = 'cell';
        for (var v = -1; v <= 10; v++) {
          var s = document.createElement('span');
          s.style.setProperty('--v', v);
          var digit = (v + 10) % 10;            /* v=-1→9（上绕接），v=10→0（下绕接） */
          s.textContent = digit;
          if (v >= 0 && v <= 9 && digit === +ch) s.classList.add('cur');
          cell.appendChild(s);
        }
        el.appendChild(cell);
      } else {
        var sep = document.createElement('span');
        sep.className = 'sep';
        sep.textContent = ch;
        el.appendChild(sep);
      }
    });
    setOdo(el, text);                 /* 官网 buildOdo 末尾自带：建完轮子立即拨到位（设 --d） */
  }
  function setOdo(el, text) {   /* 官网用 $$('.cell', el) 只认数字格——children 混着 sep 会错位 */
    var cells = $$('.cell', el), ci = 0;
    String(text).split('').forEach(function (ch) {
      if (!/\d/.test(ch)) return;
      var cell = cells[ci]; ci++;
      if (!cell) return;
      cell.style.setProperty('--d', +ch);
      $$('span', cell).forEach(function (s) {
        s.classList.toggle('cur', parseFloat(s.style.getPropertyValue('--v')) === +ch);
      });
    });
  }

  /* ══════════════════ 4. 上屏绘制 paint()（§10 + §3 联动）══════════════════ */
  var DRUM_IDX = { win: 1, scr: 2, sys: 3, gen: 4 };
  var TRACK_IDX = { win: 0, scr: 1, sys: 2, gen: 3 };

  function paintDot(key, on, dim, value) {   /* 官网三态：on / dim / 尾部读数 value */
    var dot = $('[data-dot="' + key + '"]');
    if (!dot) return;
    dot.classList.toggle('on', !!on);
    dot.classList.toggle('dim', !!dim);
    var b = dot.querySelector('b');
    if (!b) return;
    b.textContent = value == null ? '' : value;
  }

  function stopLabel(gearKey, val) {
    var el = $('[data-gear="' + gearKey + '"]');
    if (!el) return '';
    var spans = $$('.stops span', el);
    var idx = GEAR_VALUES[gearKey].indexOf(val);
    return spans[idx] ? spans[idx].textContent : '';
  }

  function dockDirty() {
    return st.dockDelay !== st.dockAppliedDelay || st.dockAnim !== st.dockAppliedAnim;
  }

  function paintStamp() {
    var btn = $('[data-stamp]');
    if (!btn) return;
    var dirty = dockDirty();
    btn.classList.toggle('armed', dirty);
    btn.disabled = !dirty;
    $('span', btn).textContent = dirty ? t('stampApply') : t('applied');
  }

  /* 印章按钮：dirty 时点击写入 applied 值、播 keyPress、.landing 落章动画 920ms */
  var stampBtn = $('[data-stamp]');
  if (stampBtn) {
    stampBtn.addEventListener('click', function () {
      if (!dockDirty()) return;
      st.dockAppliedDelay = st.dockDelay;
      st.dockAppliedAnim = st.dockAnim;
      play('keyPress');
      paint();
      stampBtn.classList.add('landing');
      setTimeout(function () { stampBtn.classList.remove('landing'); }, 920);
    });
  }

  function awakeText() {
    if (!st.awake) return '0:00';
    if (st.awakeDuration >= 485) return '∞';
    var h = Math.floor(st.awakeDuration / 60);
    var m = Math.round(st.awakeDuration % 60);
    return h + ':' + (m < 10 ? '0' + m : m);
  }

  function paint() {
    /* 模式联动：鼓轮 / 传送带 / 甲板 / 屏体 / 模式键 */
    var m = st.mode;
    var drum = $('.pt-drum');
    if (drum) drum.style.setProperty('--i', DRUM_IDX[m]);
    $$('.pt-drum i', drum || document).forEach(function (i) {
      i.classList.toggle('cur', i.dataset.mode === m && i === drum.children[DRUM_IDX[m]]);
    });
    var track = $('.pt-track');
    if (track) track.style.setProperty('--i', TRACK_IDX[m]);
    $$('.pt-deck').forEach(function (d) {
      var on = d.dataset.deck === m;
      d.toggleAttribute('inert', !on);
      d.setAttribute('aria-hidden', on ? 'false' : 'true');
    });
    $$('.pt-sc-body').forEach(function (b) {
      b.classList.toggle('cur', b.dataset.body === m);
    });
    $$('.pt-key').forEach(function (k) {
      var on = k.dataset.mode === m;
      k.setAttribute('aria-checked', on ? 'true' : 'false');
      k.tabIndex = on ? 0 : -1;
    });

    /* I/O 开关 */
    $$('.pt-sw[data-sw]').forEach(function (sw) {
      sw.setAttribute('aria-checked', st[sw.dataset.sw] ? 'true' : 'false');
    });

    /* 从属行灰显 */
    $$('[data-parent]').forEach(function (el) {
      el.classList.toggle('pt-off', !st[el.dataset.parent]);
    });

    /* 屏体读数点（官网 §10 逐键规格：on / dim / 尾部 value） */
    paintDot('smooth', st.smooth);
    paintDot('reverse', st.reverse, !st.smooth);
    paintDot('horiz', st.horiz !== 'off', !st.smooth, MOD_SYM[st.horiz]);
    paintDot('precise', st.precise !== 'off', !st.smooth, MOD_SYM[st.precise]);
    paintDot('shake', st.shake);
    paintDot('dockMin', st.dockMin);
    paintDot('dockPrev', st.dockPrev);
    paintDot('wless', st.wless, !st.dockPrev);
    paintDot('cmdTab', st.cmdTab);
    paintDot('hideAll', st.hideAll, false, st.hideAll ? SHORTCUT : '');
    paintDot('anim', st.anim);
    paintDot('dockIcon', st.dockIcon);
    paintDot('sound', st.soundSet !== 0, false, st.soundSet === 0 ? '' : t('set' + st.soundSet));
    paintDot('awake', st.awake);
    var delayStops = $$('.stops span', $('[data-gear="dockDelay"]'));
    var animStops = $$('.stops span', $('[data-gear="dockAnim"]'));
    paintDot('dockDelay', st.dockDelay !== 2, false, delayStops[st.dockDelay] ? delayStops[st.dockDelay].textContent : '');
    paintDot('dockAnim', st.dockAnim !== 1, false, animStops[st.dockAnim] ? animStops[st.dockAnim].textContent : '');
    paintDot('dockPending', dockDirty(), false, dockDirty() ? t('stampApply') : t('nothing'));

    /* 推子 */
    function faderFrac(el) {
      var min = +el.dataset.min, max = +el.dataset.max;
      return (st[el.dataset.fader] - min) / (max - min);
    }
    $$('.pt-fader').forEach(function (el) {
      var f = Math.min(1, Math.max(0, faderFrac(el)));
      el.style.setProperty('--f', f);
      el.setAttribute('aria-valuenow', st[el.dataset.fader]);
      var key = el.dataset.fader;
      if (key === 'speed') el.setAttribute('aria-valuetext', st.speed.toFixed(2) + '×');
      else if (key === 'undo') el.setAttribute('aria-valuetext', st.undo === 0 ? t('undoOff') : t('undoVal').replace('{n}', st.undo.toFixed(1)));
      else if (key === 'awakeDuration') el.setAttribute('aria-valuetext', st.awakeDuration === 0 ? t('awakeOff') : (st.awakeDuration >= 485 ? t('awakeNoLimit') : t('awakeMinutes').replace('{n}', st.awakeDuration)));
    });

    /* 挡位选择器 */
    $$('.pt-gear').forEach(function (el) {
      var key = el.dataset.gear;
      var sel = GEAR_VALUES[key].indexOf(st[key]);
      el.style.setProperty('--s', sel);
      el.setAttribute('aria-valuenow', sel);
      var spans = $$('.stops span', el);
      el.setAttribute('aria-valuetext', spans[sel] ? spans[sel].textContent : '');
      spans.forEach(function (s, i) { s.classList.toggle('cur', i === sel); });
      /* 互斥锁：对方占着的修饰键挡变灰（off 永不锁） */
      if (el.dataset.lockable) {
        var other = key === 'horiz' ? st.precise : st.horiz;
        spans.forEach(function (s, i) {
          var locked = MODS[i] !== 'off' && MODS[i] === other;
          s.style.color = locked ? 'var(--pt-ink2)' : '';
          s.style.opacity = locked ? '.35' : '';
        });
        $$('.pt-rail i', el).forEach(function (tk, i) {
          tk.style.opacity = (MODS[i] !== 'off' && MODS[i] === other) ? '.4' : '';
        });
      }
      /* 读数 */
      var val = $('[data-val="' + key + '"]');
      if (val) {
        if (key === 'horiz' || key === 'precise') {
          val.textContent = st[key] === 'off' ? '' : MOD_SYM[st[key]] + ' ' + MOD_LABEL[st[key]];
        } else if (key === 'lang') {
          val.textContent = st.lang === 'system' ? t('try.gen.langSystemTitle') : LANG_TITLE[st.lang];
        } else {
          val.textContent = spans[sel] ? spans[sel].textContent : '';
        }
      }
      if (key === 'lang') {
        var etch = $('[data-etch="lang"]');
        if (etch) etch.textContent = st.lang === 'system' ? t('try.gen.langSystemTitle') : LANG_TITLE[st.lang];
      }
    });

    /* 里程表 */
    var odoSpeed = $('[data-odo="speed"]');
    if (odoSpeed) setOdo(odoSpeed, st.speed.toFixed(2));
    var odoUndo = $('[data-odo="undo"]');
    if (odoUndo) setOdo(odoUndo, st.undo.toFixed(1));
    var odoAwake = $('[data-odo="awake"]');
    if (odoAwake) {
      var txt = awakeText();
      if (odoAwake.dataset.text !== txt) { buildOdo(odoAwake, txt); odoAwake.dataset.text = txt; }
      else setOdo(odoAwake, txt);
    }

    /* 音效旋钮 */
    var knob = $('.pt-knob');
    if (knob) {
      var az = { 0: 120, 1: 60, 2: 0 }[st.soundSet];
      $('.cap .dot', knob).style.setProperty('--az', az);
      knob.setAttribute('aria-valuenow', st.soundSet);
      $$('.d', knob).forEach(function (d) { d.classList.toggle('cur', +d.dataset.set === st.soundSet); });
    }

    /* 印章按钮 */
    paintStamp();
  }

  /* ══════════════════ 5. 模式切换（§3）══════════════════ */
  function selectMode(m) {
    if (m === st.mode) return;
    st.mode = m;
    paint();
    play('modeChange');            /* 声音来自滚筒不是键 */
  }

  $$('.pt-key[data-mode]').forEach(function (k) {
    k.addEventListener('click', function () { selectMode(k.dataset.mode); });
    k.addEventListener('keydown', function (ev) {
      var d = { ArrowLeft: -1, ArrowRight: 1 }[ev.key];
      if (!d) return;
      ev.preventDefault();
      var i = MODES.indexOf(st.mode) + d;
      if (i < 0 || i >= MODES.length) return;
      selectMode(MODES[i]);
      $$('.pt-key').forEach(function (b) { if (b.dataset.mode === MODES[i]) b.focus(); });
    });
  });

  /* ══════════════════ 6. 拖拽基元（pointer capture + __ptDrag）══════════════════ */
  function startDrag(el, ev) {
    el.__ptDrag = true;
    if (el.setPointerCapture) { try { el.setPointerCapture(ev.pointerId); } catch (e) {} }
  }
  function endDrag(el) { el.__ptDrag = false; }
  /* 官网 §8.5：松手可能发生在任何地方（指针已离开控件），统一在 window 上以 capture 清掉全部拖拽标志 */
  ['pointerup', 'pointercancel'].forEach(function (type) {
    window.addEventListener(type, function () {
      $$('.pt-fader, .pt-gear').forEach(function (el) { el.__ptDrag = false; });
    }, true);
  });

  /* ══════════════════ 7. I/O 开关（§4）══════════════════ */
  $$('.pt-sw[data-sw]').forEach(function (sw) {
    sw.addEventListener('click', function () {
      if (sw.closest('.pt-off')) return;
      var key = sw.dataset.sw;
      var lands = !st[key];
      st[key] = lands;
      if (key === 'awake') {
        if (lands && st.awakeDuration === 0) st.awakeDuration = 485;
        if (!lands) st.awakeDuration = 0;
      }
      paint();
      play(lands ? 'switchOn' : 'switchOff');
    });
  });

  /* ══════════════════ 8. 推子（§5）══════════════════ */
  function snapFader(el, raw) {
    var min = +el.dataset.min, max = +el.dataset.max, step = +el.dataset.step;
    var snapped = Math.round((raw - min) / step) * step + min;
    snapped = Math.min(max, Math.max(min, snapped));
    snapped = Math.round(snapped * 1000) / 1000;      /* 浮点修圆 3 位 */
    return snapped;
  }
  function faderFromPointer(el, ev) {
    var r = el.getBoundingClientRect();
    var frac = (ev.clientX - r.left) / r.width;
    var min = +el.dataset.min, max = +el.dataset.max;
    return min + Math.min(1, Math.max(0, frac)) * (max - min);
  }
  $$('.pt-fader').forEach(function (el) {
    var key = el.dataset.fader;
    function commit(raw, tickSlot) {
      var next = snapFader(el, raw);
      if (next === st[key]) return;
      /* 跨过档位线时 tick（防连响） */
      var step = +el.dataset.step;
      var prevIdx = Math.round((st[key] - +el.dataset.min) / step);
      var nextIdx = Math.round((next - +el.dataset.min) / step);
      st[key] = next;
      if (key === 'awakeDuration' && next !== 0) st.awake = true;
      paint();
      if (Math.abs(nextIdx - prevIdx) >= 1 && tickSlot) {
        var g = antiRattle('fader-' + tickSlot);
        if (g) play(tickSlot, g);
      }
    }
    el.addEventListener('pointerdown', function (ev) {
      if (el.closest('.pt-off')) return;
      startDrag(el, ev);
      el.focus();
      commit(faderFromPointer(el, ev), 'faderTick');
    });
    el.addEventListener('pointermove', function (ev) {
      if (!el.__ptDrag) return;
      commit(faderFromPointer(el, ev), 'faderTick');
    });
    el.addEventListener('pointerup', function () {
      if (!el.__ptDrag) return;
      endDrag(el);          /* 跟手走：值已在 move 里落位，松手只收尾 */
    });
    el.addEventListener('keydown', function (ev) {
      var d = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 }[ev.key];
      if (!d || el.closest('.pt-off')) return;
      ev.preventDefault();
      var step = +el.dataset.step;
      var next = snapFader(el, st[key] + d * step);
      if (next === st[key]) return;
      st[key] = next;
      if (key === 'awakeDuration' && next !== 0) st.awake = true;
      paint();
      play('faderStep');
    });
  });

  /* ══════════════════ 9. 挡位选择器（§6）══════════════════ */
  function gearAvailable(el, i) {
    if (!el.dataset.lockable) return true;
    var other = el.dataset.gear === 'horiz' ? st.precise : st.horiz;
    return MODS[i] === 'off' || MODS[i] !== other;
  }
  function recoil(el, i) {
    if (REDUCED) return;
    var k = $('.pt-rail .k', el);
    var sel = GEAR_VALUES[el.dataset.gear].indexOf(st[el.dataset.gear]);
    k.style.transform = 'translateX(' + (i >= sel ? 3 : -3) + 'px)';
    setTimeout(function () { k.style.transform = ''; }, 90);
  }
  function gearSelect(el, i) {
    var key = el.dataset.gear;
    var list = GEAR_VALUES[key];
    if (i < 0 || i >= list.length) return;
    if (list[i] === st[key]) return;                  /* 停在原地 = 不动也不响 */
    if (!gearAvailable(el, i)) { recoil(el, i); return; }   /* 拒绝是静默的 */
    st[key] = list[i];
    paint();
    play('gearDetent');
  }
  $$('.pt-gear').forEach(function (el) {
    var count = +el.dataset.count;
    function fromPointer(ev) {
      var r = el.querySelector('.pt-rail').getBoundingClientRect();
      return Math.min(count - 1, Math.max(0, Math.floor((ev.clientX - r.left) / (r.width / count))));
    }
    el.addEventListener('pointerdown', function (ev) {
      if (el.closest('.pt-off')) return;
      startDrag(el, ev);
      el.focus();
      gearSelect(el, fromPointer(ev));
    });
    el.addEventListener('pointermove', function (ev) {
      if (!el.__ptDrag) return;
      gearSelect(el, fromPointer(ev));
    });
    el.addEventListener('keydown', function (ev) {
      var d = { ArrowLeft: -1, ArrowRight: 1, ArrowDown: -1, ArrowUp: 1 }[ev.key];
      if (!d || el.closest('.pt-off')) return;
      ev.preventDefault();
      var list = GEAR_VALUES[el.dataset.gear];
      var i = list.indexOf(st[el.dataset.gear]) + d;
      while (i >= 0 && i < list.length && !gearAvailable(el, i)) i += d;   /* 跨过锁死的挡 */
      gearSelect(el, i);
    });
  });

  /* ══════════════════ 10. 音效旋钮（§7）══════════════════ */
  function setSoundSet(next) {
    if (next === st.soundSet) return;
    st.soundSet = next;
    paint();
    var wait = REDUCED ? 0 : 120;
    setTimeout(function () { play('keyPress'); }, wait);
  }
  var knobEl = $('.pt-knob');
  if (knobEl) {
    $('.cap', knobEl).addEventListener('click', function () {
      setSoundSet((st.soundSet + 1) % 3);           /* 0→1→2→0 前进一位 */
    });
    $$('.d', knobEl).forEach(function (d) {
      d.addEventListener('click', function () { setSoundSet(+d.dataset.set); });
    });
    knobEl.addEventListener('keydown', function (ev) {
      var d = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 }[ev.key];
      if (!d) return;
      ev.preventDefault();
      var next = st.soundSet + d;
      if (next < 0 || next > 2) return;             /* 两端夹住不绕圈 */
      setSoundSet(next);
    });
  }

  /* ══════════════════ 11. VU 活动条（§9）══════════════════ */
  var vuLevel = 0, vuLast = performance.now();
  var vuEls = $$('.pt-vu i');
  function vuFrame(now) {
    var dt = (now - vuLast) / 1000;
    vuLast = now;
    vuLevel = Math.max(0, vuLevel - 1.65 * dt);
    var lit = Math.round(vuLevel);
    vuEls.forEach(function (el, i) { el.classList.toggle('on', i < lit); });
    requestAnimationFrame(vuFrame);
  }
  window.addEventListener('wheel', function (ev) {
    if (st.mode !== 'scr') return;
    var rig = $('#ptRig');
    if (!rig) return;
    var r = rig.getBoundingClientRect();
    var visible = r.bottom > 0 && r.top < innerHeight;
    if (!visible) return;
    vuLevel = Math.min(12, vuLevel + 0.22);
  }, { passive: true });
  requestAnimationFrame(vuFrame);

  /* ══════════════════ 11.5 滚动棘轮声（官网 scroll-sfx.js 同参数）═════════════════
     页面滚动推一把虚拟轮子 → 带阻尼自转 → 每转过 15° 合成一格棘齿。
     合成音 = 4300Hz 带通噪声瞬态 + 1050Hz 三角波敲击（非 wav）。 */
  var WHEEL_GAIN = 0.010, WHEEL_MAX = 2.4, DAMP = 0.95;
  var TICK_STEP = Math.PI * 2 / 24, MIN_GAP = 0.018, IDLE_MUTE = 4;
  var TRIM_TICK = 2.4697;             /* 官网 hero-scene TRIM['synth:tick'] 同值 */
  var sfxVel = 0, sfxAccum = 0, sfxSimT = 0;
  var sfxLastInteract = -Infinity, sfxLastTick = -1, sfxLastY = null, sfxRaf = 0, sfxLastFrame = 0;
  function sfxNoiseBuf(c) {
    if (!c.__pommeNoise) {
      var n = Math.ceil(c.sampleRate * 0.05);
      var buf = c.createBuffer(1, n, c.sampleRate);
      var d = buf.getChannelData(0);
      for (var i = 0; i < n; i++) d[i] = Math.random() * 2 - 1;
      c.__pommeNoise = buf;
    }
    return c.__pommeNoise;
  }
  function sfxTick(level) {
    if (!waCtx || waCtx.state !== 'running') return;
    var c = waCtx, when = c.currentTime;
    var g = c.createGain(); g.gain.value = level * TRIM_TICK; g.connect(waMaster);
    var rate = 1 + (Math.random() * 2 - 1) * 0.05;
    var src = c.createBufferSource();
    src.buffer = sfxNoiseBuf(c); src.playbackRate.value = rate;
    var bp = c.createBiquadFilter();
    bp.type = 'bandpass'; bp.frequency.value = 4300 * rate; bp.Q.value = 3.0;
    var ng = c.createGain();
    ng.gain.setValueAtTime(1.0, when);
    ng.gain.exponentialRampToValueAtTime(0.001, when + 0.011);
    src.connect(bp).connect(ng).connect(g); src.start(when);
    var osc = c.createOscillator();
    osc.type = 'triangle'; osc.frequency.value = 1050 * rate;
    var og = c.createGain();
    og.gain.setValueAtTime(0.22, when);
    og.gain.exponentialRampToValueAtTime(0.001, when + 0.009);
    osc.connect(og).connect(g); osc.start(when); osc.stop(when + 0.014);
  }
  function sfxEmit(dAngle, dt) {
    var mag = Math.abs(dAngle);
    if (mag <= 1e-6) return;
    if (sfxSimT - sfxLastInteract > IDLE_MUTE) { sfxAccum = 0; return; }
    var speed = mag / Math.max(dt, 1e-3);
    sfxAccum += mag;
    if (sfxAccum < TICK_STEP) return;
    sfxAccum = Math.min(sfxAccum, TICK_STEP * 2);
    if (sfxSimT - sfxLastTick < MIN_GAP) return;
    sfxLastTick = sfxSimT;
    sfxAccum -= TICK_STEP;
    sfxTick(1.00 * Math.max(0.68, Math.min(1, 1 - speed * 0.016)));
  }
  var sfxAngle = 0;
  function sfxFrame(t) {
    sfxRaf = 0;
    var dt = Math.min(0.05, sfxLastFrame ? (t - sfxLastFrame) / 1000 : 0.016);
    sfxLastFrame = t;
    sfxSimT += dt;
    var prev = sfxAngle;
    sfxAngle += sfxVel * dt;
    sfxVel *= Math.pow(DAMP, dt * 60);
    if (Math.abs(sfxVel) < 0.01) sfxVel = 0;
    sfxEmit(sfxAngle - prev, dt);
    if (sfxVel !== 0) sfxSchedule(); else sfxLastFrame = 0;
  }
  function sfxSchedule() { if (!sfxRaf) sfxRaf = requestAnimationFrame(sfxFrame); }
  window.addEventListener('scroll', function () {
    var y = window.scrollY || document.documentElement.scrollTop || 0;
    if (sfxLastY == null) { sfxLastY = y; return; }
    var dy = y - sfxLastY; sfxLastY = y;
    if (dy === 0) return;
    if (waCtx && waCtx.state === 'suspended') waCtx.resume().catch(function () {});
    sfxVel = Math.max(-WHEEL_MAX, Math.min(WHEEL_MAX, sfxVel + dy * WHEEL_GAIN));
    sfxLastInteract = sfxSimT;
    sfxSchedule();
  }, { passive: true });

  /* ══════════════════ 12. 电源拨杆（§12）══════════════════ */
  var powerLever = $('.pt-lever');
  if (powerLever) {
    powerLever.addEventListener('click', function () {
      if (powerLever.classList.contains('thrown')) return;
      powerLever.classList.add('thrown');
      play('powerThrow');
      setTimeout(function () { powerLever.classList.remove('thrown'); }, 2000);
    });
  }

  /* ══════════════════ 13. 更新按钮演示（§11 尾）══════════════════ */
  var updateBtn = $('[data-update-check]');
  if (updateBtn) {
    updateBtn.addEventListener('click', function () {
      if (updateBtn.disabled) return;
      updateBtn.disabled = true;
      var title = $('[data-update-title]');
      var detail = $('[data-update-detail]');
      var lamp = $('.pt-update-row .pt-status-lamp');
      if (title) title.textContent = t('updateChecking');
      if (detail) detail.textContent = t('updateCheckingD');
      if (lamp) lamp.classList.add('on');
      setTimeout(function () {
        if (title) title.textContent = t('updateCurrent');
        if (detail) detail.textContent = t('updateChecked');
        updateBtn.disabled = false;
        setTimeout(function () { if (lamp) lamp.classList.remove('on'); }, 1600);
      }, 850);
    });
  }

  /* ══════════════════ 14. 许可证激活仪式（§11）══════════════════ */
  var licenseInput = $('#ptLicenseKey');
  var licenseStatus = $('#ptLicenseStatus');
  var licenseSec = $('.pt-license');
  var actLever = $('[data-license-activate]');
  var relLever = $('[data-license-release]');

  function sanitizeKey(raw) {
    var s = raw.toUpperCase().replace(/[\s-]/g, '').replace(/I/g, '1').replace(/L/g, '1').replace(/O/g, '0');
    s = s.replace(/[^0-9A-HJKMNP-TV-Z]/g, '').slice(0, 16);
    var groups = [];
    for (var i = 0; i < s.length; i += 4) groups.push(s.slice(i, i + 4));
    return groups.join('-');
  }
  function keyFilled() {
    return licenseInput && licenseInput.value.replace(/-/g, '').length === 16;
  }
  function setLicenseStatus(text, ok) {
    if (!licenseStatus) return;
    licenseStatus.textContent = text || '\u00A0';
    licenseStatus.classList.toggle('ok', !!ok);
  }
  function maskKey() {
    var s = licenseInput.value.replace(/-/g, '');
    if (s.length < 8) return s;
    return s.slice(0, 4) + '-••••-••••-' + s.slice(-4);
  }
  function setActivatedCopy() {
    var d = $('[data-license-key-mask]');
    if (d) d.textContent = maskKey() + ' · ' + (LANG === 'zh'
      ? '绑定 5 台 · 已用 1 台 · 本机是第 1 台'
      : '5 Macs · 1 in use · this Mac is #1');
  }
  function showTrial() {
    if (licenseInput) licenseInput.value = '';
    if (licenseSec) licenseSec.setAttribute('data-license-state', 'trial');
    var trial = $('[data-license-face="trial"]');
    var active = $('[data-license-face="active"]');
    if (trial) trial.hidden = false;
    if (active) active.hidden = true;
    if (actLever) { actLever.classList.remove('powered', 'latched'); setLicenseLever(actLever, 'activate', 0, false); }
    setLicenseStatus('');
  }

  /* 拉杆 travel：0→1（activate 正向；release 反向，就位 = 1） */
  function setLicenseLever(el, mode, travel, animate) {
    var max = el.clientWidth - 60;
    if (max <= 0) max = el.offsetWidth - 60 || 448;
    var x = Math.min(1, Math.max(0, travel)) * max;
    el.style.setProperty('--pt-handle-x', x + 'px');
    el.style.setProperty('--pt-fill', x + 'px');
    el.__ptTravel = travel;
    el.__ptMode = mode;
    if (!animate) {
      var h = el.querySelector('.handle');
      h.style.transition = 'none';
      void h.offsetWidth;
      h.style.transition = '';
    }
  }

  if (licenseInput) {
    licenseInput.addEventListener('input', function () {
      var before = licenseInput.value;
      var caretAtEnd = licenseInput.selectionStart === before.length;
      var clean = sanitizeKey(before);
      if (clean !== before) {
        licenseInput.value = clean;
        if (caretAtEnd) licenseInput.setSelectionRange(clean.length, clean.length);
      }
      var filled = keyFilled();
      var wasPowered = actLever.classList.contains('powered');
      if (filled && !wasPowered) {
        actLever.classList.add('powered');
        actLever.classList.remove('power-flash');
        void actLever.offsetWidth;
        if (!REDUCED) {
          actLever.classList.add('power-flash');
          setTimeout(function () { actLever.classList.remove('power-flash'); }, 520);
        }
      } else if (!filled && wasPowered) {
        actLever.classList.remove('powered');
      }
      setLicenseStatus('');
    });
  }

  var SELF_TEST_TIMING = [150, 340, 530, 720, 910];

  /* 官网 LicenseSelfTest.swift 两条纯函数逐式照搬：返回连续亮度 0.10..1（不是开关） */
  function selfTestLamp(lamp, t) {
    if (t < .10) return 1;
    if (t >= .62) return .10;
    var head = ((t - .10) / .52) * 14;
    var distance = head - lamp;
    if (distance < 0 || distance >= 2.5) return .10;
    return 1 - distance / 2.5 * .9;
  }
  function selfTestNeedleFill(t) {
    if (t < .55) return 0;
    var progress = (t - .55) / .45;
    return progress < .45
      ? Math.sin(progress / .45 * Math.PI / 2)
      : Math.max(0, 1 - (progress - .45) / .55);
  }
  function runSelfTestLamps(duration, done) {
    var gen = $('[data-license-self-test]');
    var scrBody = $('.pt-sc-body.gen');
    var lamps = $$('.pt-self-test-lamps i', gen);
    var needle = $$('.pt-self-test-needle i', gen);
    var t0 = performance.now();
    if (scrBody) scrBody.classList.add('testing');
    if (gen) gen.setAttribute('aria-hidden', 'false');
    function frame(now) {
      var t = (now - t0) / duration;
      if (t >= 1) {
        lamps.forEach(function (l) { l.style.backgroundColor = ''; l.style.boxShadow = ''; });
        needle.forEach(function (n) { n.classList.remove('on'); });
        if (scrBody) scrBody.classList.remove('testing');
        if (gen) gen.setAttribute('aria-hidden', 'true');
        done();
        return;
      }
      lamps.forEach(function (l, i) {   /* 官网：color-mix 连续亮度 + boxShadow 辉光 */
        var lit = REDUCED ? .75 : selfTestLamp(i, t);
        l.style.backgroundColor = 'color-mix(in srgb, var(--pt-sc-accent) ' + (12 + 88 * lit) + '%, transparent)';
        l.style.boxShadow = '0 0 ' + (3 * lit) + 'px color-mix(in srgb, var(--pt-sc-accent) ' + (70 * lit) + '%, transparent)';
      });
      var fill = selfTestNeedleFill(t);
      var lit = Math.round(fill * needle.length);
      needle.forEach(function (n, i) { n.classList.toggle('on', i < lit); });
      requestAnimationFrame(frame);
    }
    requestAnimationFrame(frame);
  }

  function runActivation() {
    if (!actLever) return;
    actLever.classList.add('throwing');
    play('activationSeat', ceremonyGain('activationSeat'));
    SELF_TEST_TIMING.forEach(function (ms) {
      setTimeout(function () { play('selfTestStep', ceremonyGain('selfTestStep')); }, ms);
    });
    runSelfTestLamps(1200, function () {
      actLever.classList.remove('throwing');
      if (licenseSec) licenseSec.setAttribute('data-license-state', 'active');
      var trial = $('[data-license-face="trial"]');
      var active = $('[data-license-face="active"]');
      if (trial) trial.hidden = true;
      if (active) active.hidden = false;
      var reading = $('.pt-license-reading.landed', active);
      if (reading && !REDUCED) {
        reading.classList.remove('landed');
        void reading.offsetWidth;
        reading.classList.add('landed');
      }
      setActivatedCopy();
      play('activationSettle', ceremonyGain('activationSettle'));
      /* 释放杆就位（release 模式，初始 travel=1） */
      if (relLever) { relLever.classList.add('latched'); setLicenseLever(relLever, 'release', 1, false); }
      setLicenseStatus(t('activationDone'), true);
    });
  }

  /* 仪式增益：第 1 套激活流程 ×1.40，继电器/成功/释放再 ×1.50；第 2 套不加成 */
  function ceremonyGain(slot) {
    if (st.soundSet !== 1) return 1;
    var g = 1.40;
    if (slot === 'selfTestStep' || slot === 'activationSettle' || slot === 'activationRelease') g *= 1.50;
    return g;
  }

  /* 激活拉杆交互：pointer 拖拽 travel 0→1，12 棘轮齿 */
  function bindLicenseLever(el, mode) {
    if (!el) return;
    var RATCHET = 12;
    var lastTooth = 0;
    function travelFromPointer(ev) {
      var r = el.getBoundingClientRect();
      var usable = r.width - 60;                     /* 手柄宽 54 + 边距 */
      return Math.min(1, Math.max(0, (ev.clientX - r.left - 3) / (usable - 3)));
    }
    el.addEventListener('pointerdown', function (ev) {
      if (el.closest('.pt-off')) return;
      if (mode === 'activate' && !keyFilled()) {
        setLicenseStatus(t('keyPrompt'));
        if (licenseInput) licenseInput.focus();
        return;
      }
      startDrag(el, ev);
      el.__ptLastTooth = Math.floor((el.__ptTravel || 0) * RATCHET);
    });
    el.addEventListener('pointermove', function (ev) {
      if (!el.__ptDrag) return;
      var tv = travelFromPointer(ev);
      var travel = mode === 'activate' ? tv : 1 - tv;
      setLicenseLever(el, mode, travel, true);
      var tooth = Math.floor(travel * RATCHET);
      if (tooth !== el.__ptLastTooth) {
        var g = antiRattle('ratchet');
        if (g) play('ratchetTooth', g);
        el.__ptLastTooth = tooth;
      }
      if (mode === 'activate' && travel >= 0.997) {
        endDrag(el);
        runActivation();
      }
      if (mode === 'release' && travel <= 0.003) {
        endDrag(el);
        releaseActivation();
      }
    });
    el.addEventListener('pointerup', function () {
      if (!el.__ptDrag) return;
      endDrag(el);
      /* 没拖到底松手弹回 */
      if (mode === 'activate') setLicenseLever(el, mode, 0, true);
      else setLicenseLever(el, mode, 1, true);
    });
    el.addEventListener('keydown', function (ev) {
      if (mode === 'activate') {
        if (ev.key === ' ') {
          ev.preventDefault();
          if (!keyFilled()) { setLicenseStatus(t('keyPrompt')); if (licenseInput) licenseInput.focus(); return; }
          var tv2 = Math.min(1, (el.__ptTravel || 0) + 0.15);
          setLicenseLever(el, mode, tv2, true);
          play('ratchetTooth');
          if (tv2 >= 0.997) runActivation();
          return;
        }
        if (ev.key === 'ArrowRight') {
          ev.preventDefault();
          if (!keyFilled()) { setLicenseStatus(t('keyPrompt')); if (licenseInput) licenseInput.focus(); return; }
          var tv3 = Math.min(1, (el.__ptTravel || 0) + 0.2);
          setLicenseLever(el, mode, tv3, true);
          play('ratchetTooth');
          if (tv3 >= 0.997) runActivation();
          return;
        }
        if (ev.key === 'ArrowLeft') {
          ev.preventDefault();
          setLicenseLever(el, mode, Math.max(0, (el.__ptTravel || 0) - 0.2), true);
          return;
        }
        if (ev.key === 'Escape') { ev.preventDefault(); setLicenseLever(el, mode, 0, true); return; }
        if (ev.key === 'Enter') {
          ev.preventDefault();
          if (!keyFilled()) { setLicenseStatus(t('keyPrompt')); if (licenseInput) licenseInput.focus(); return; }
          runActivation();
        }
      } else {
        if (ev.key === 'ArrowLeft' || ev.key === ' ') {
          ev.preventDefault();
          var tv4 = Math.max(0, (el.__ptTravel == null ? 1 : el.__ptTravel) - (ev.key === ' ' ? 0.15 : 0.2));
          setLicenseLever(el, mode, tv4, true);
          play('ratchetTooth');
          if (tv4 <= 0.003) releaseActivation();
        }
        if (ev.key === 'ArrowRight') {
          ev.preventDefault();
          setLicenseLever(el, mode, Math.min(1, (el.__ptTravel || 0) + 0.2), true);
        }
        if (ev.key === 'Escape') { ev.preventDefault(); setLicenseLever(el, mode, 1, true); }
      }
    });
  }
  bindLicenseLever(actLever, 'activate');
  bindLicenseLever(relLever, 'release');

  function releaseActivation() {
    play('activationSeat', ceremonyGain('activationSeat'));
    setTimeout(function () {
      play('activationRelease', ceremonyGain('activationRelease'));
      showTrial();
    }, 160);
  }

  /* ══════════════════ 15. 反馈传真机 FX-01（fax-screens-analysis + fax-copy）══════════════════ */
  var fxOnLang = function () {};       /* 语言切换后的重绘钩子（Fax 块内赋值） */
  var fx = $('[data-fx]');
  if (fx) {
    var fxStateZh = $('[data-fx-state-zh]', fx);
    var fxStateEn = $('[data-fx-state-en]', fx);
    var fxCountEl = $('[data-fx-count]', fx);
    var fxText = $('[data-fx-text]', fx);
    var fxEmail = $('[data-fx-email]', fx);
    var fxNo = $('[data-fx-no]', fx);
    var fxDateEl = $('[data-fx-date]', fx);
    var fxPaper = $('[data-fx-paper]', fx);
    var fxStampEl = $('[data-fx-stamp]', fx);
    var fxStampText = $('[data-fx-stamp-text]', fx);
    var fxMlang = $('[data-fx-mlang]', fx);
    var fxReceipt = $('[data-fx-receipt]', fx);
    var fxSendBtn = $('[data-fx-send]', fx);
    var fxClearBtn = $('[data-fx-clear]', fx);
    var fxTrigger = $('[data-fx-attach-sw]', fx);
    var fxModeBtns = $$('.fx-mode', fx);

    var fxPhase = 'ready';
    var fxSerial = 881;                 /* 纸面单号，每完成一次传送 +1（新参考组从 NO.0881 起） */
    var fxMode = 'bug';
    var fxT0 = 0;                      /* 发送起点（回执「用时」用） */
    var FX_WEEK_ZH = ['周日', '周一', '周二', '周三', '周四', '周五', '周六'];
    var FX_WEEK_EN = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
    var FX_BUSY_PHASES = ['offhook', 'dialing', 'connecting', 'sending', 'sent', 'printed', 'tearing', 'loading', 'busy'];

    function fxPad(n, w) { var s = String(n); while (s.length < w) s = '0' + s; return s; }
    function fxNow() {
      var d = new Date();
      return {
        date: d.getFullYear() + '-' + fxPad(d.getMonth() + 1, 2) + '-' + fxPad(d.getDate(), 2),
        stamp: fxPad(d.getMonth() + 1, 2) + '-' + fxPad(d.getDate(), 2) + ' ' + fxPad(d.getHours(), 2) + ':' + fxPad(d.getMinutes(), 2),
        week: LANG === 'zh' ? FX_WEEK_ZH[d.getDay()] : FX_WEEK_EN[d.getDay()]
      };
    }
    /* LCD 右下小行读数（机器刻字，不翻译） */
    function fxEnLine(p) {
      var n = fxNow();
      switch (p) {
        case 'ready': return 'READY · ' + n.stamp + ' ' + n.week;
        case 'offhook': return 'OFF HOOK';
        case 'dialing': return 'DIALING · POMME-TOYS';
        case 'connecting': return 'CONNECT · 14.4K';
        case 'sending': return 'SENDING · P.01 · ' + (fx._pct || 10) + '%';
        case 'sent': return 'TRANSMISSION OK';
        case 'printed': return 'TEAR OFF · NO.' + fxPad(fxSerial, 4);
        case 'loading': return 'LOADING · NO.' + fxPad(fxSerial, 4);
        default: return '';
      }
    }
    function fxShow(p) {
      if (fxStateZh) fxStateZh.textContent = t('fx.st.' + p);
      if (fxStateEn) fxStateEn.textContent = fxEnLine(p);
    }
    var fxSfxTimers = [];                 /* 音效循环集中登记，相位切换统一清（防多轮抢播） */
    var fxSfxHold = false;                /* 主动编排链中豁免清理（fxSetPhase 会误杀刚排的下一步音） */
    function fxSfxClear() {
      fxSfxTimers.forEach(function (t) { clearInterval(t); clearTimeout(t); });
      fxSfxTimers = [];
    }
    function fxSfxIv(fn, ms) { var iv = setInterval(fn, ms); fxSfxTimers.push(iv); return iv; }
    function fxSfxTo(fn, ms) { var to = setTimeout(fn, ms); fxSfxTimers.push(to); return to; }
    function fxSetPhase(p) {
      if (p !== fxPhase && !fxSfxHold) fxSfxClear();  /* 相位切换清音（主动编排链 fxSfxHold 时不误杀） */
      fxPhase = p;
      fx.setAttribute('data-fx-phase', p);
      fxShow(p);
      var busy = FX_BUSY_PHASES.indexOf(p) >= 0;
      if (busy) fxPrintRestore();                    /* 纸进机器时信息必须完整 */
      if (fxSendBtn) fxSendBtn.disabled = busy;
      if (fxClearBtn) fxClearBtn.disabled = busy;
      if (fxTrigger) fxTrigger.disabled = busy;
      fxModeBtns.forEach(function (b) { b.disabled = busy; });
      if (fxText) fxText.disabled = busy;
      if (fxEmail) fxEmail.disabled = busy;
      if (fxReceipt) fxReceipt.setAttribute('aria-hidden', p === 'sent' || p === 'printed' ? 'false' : 'true');
    }

    function fxChars() { return fxText ? fxText.value.length : 0; }
    function fxCountUpdate() {
      if (fxCountEl) fxCountEl.textContent = fxPad(fxChars(), 4) + ' / 2,000';
    }
    if (fxText) fxText.addEventListener('input', fxCountUpdate);

    function fxPaintStamp() {
      var bug = fxMode === 'bug';
      if (fxStampEl) fxStampEl.classList.toggle('sug', !bug);
      if (fxStampText) fxStampText.textContent = t(bug ? 'fx.stampBug' : 'fx.stampSug');
    }
    fxModeBtns.forEach(function (k) {
      k.addEventListener('click', function () {
        if (fxPhase !== 'ready') return;
        fxMode = k.dataset.fxMode;
        fxModeBtns.forEach(function (b) {
          var on = b === k;
          b.classList.toggle('cur', on);
          b.setAttribute('aria-checked', on ? 'true' : 'false');
          b.tabIndex = on ? 0 : -1;
        });
        fxPaintStamp();
      });
    });

    /* 扳机开关：机器信息框显隐（只切可见性，不塌高度；CSS 见 .fx-minfo-clip）。
       关态书写区放大高度不写死——各宽度布局（≥720px 时 minfo 130px、620px 时 81px）
       尺寸不同，固定值必然在某些宽度错位。
       配平模型：开态基准（input 自然高 / 纸满高）只在【首次满文字】时记录一次，
       之后不再更新（打印中途的纸高偏矮，不能当基准）。
       关 = 脱流框 → input 回基准高 → 量纸面缺口 → 补高缺口。
       开 = input 回基准高 → 打字机重打（钉子保证 minfo 满高）。 */
    var fxOnBase = null;                 /* { inputH, paperH } 开态基准，一次定格 */
    function fxOnBaseReady() {
      if (fxOnBase) return true;
      var input = $('.fx-input', fx), paper = $('.fx-paper', fx);
      if (!input || !paper) return false;
      /* 只有打字机满文字状态才可记基准：未打完时 minfo 内容矮，基准会污染 */
      if (!fxMinfoFullOK()) return false;
      input.style.height = '';
      fxOnBase = { inputH: input.getBoundingClientRect().height,
                   paperH: paper.getBoundingClientRect().height };
      return true;
    }
    function fxOffBalance() {
      /* off 态：input 先回开态基准高，再按纸面缺口补高 */
      var input = $('.fx-input', fx), paper = $('.fx-paper', fx);
      if (!fxOnBase || !input || !paper) return;
      input.style.height = fxOnBase.inputH + 'px';
      var d = fxOnBase.paperH - paper.getBoundingClientRect().height;
      if (Math.abs(d) > 0.5) input.style.height = (fxOnBase.inputH + d) + 'px';
    }
    if (fxTrigger) {
      fxTrigger.addEventListener('click', function () {
        if (fxPhase !== 'ready') return;
        var on = fxTrigger.getAttribute('aria-checked') !== 'true';
        fxPrintUnpin();                             /* 先无条件撤钉：off 态 clip 脱流看不见，
                                                       残留的 min-height 会让它回 on 复流时矮一截 */
        fxTrigger.setAttribute('aria-checked', on ? 'true' : 'false');
        if (!on) {
          /* 先停止打印并恢复满文字（此刻仍在 on 布局：clip 流内、文字满、
             minfo 自然满高）——这样无论打印进行到哪，基准都量得到正确值 */
          fxPrintRestore();
          fxOnBaseReady();
          fx.setAttribute('data-fx-attach', 'off');
          fxOffBalance();
        } else {
          fx.setAttribute('data-fx-attach', 'on');
          var input = $('.fx-input', fx);
          if (input && fxOnBase) input.style.height = fxOnBase.inputH + 'px';
          fxPrintMinfo();                            /* 打完时 done() 会记基准 */
        }
      });
    }

    /* 机器信息打字机：显示时逐字打印，伴随 fax-print 噼里啪啦（打印机风格）。
       空行会把框压矮、页面跟着先降后升——打印期间用满高 min-height 钉住，全程高度不变 */
    var fxPrintTk = 0;
    var fxMinfoFullH = null;             /* 满高首次量测缓存（布局无关） */
    var fxMinfoNodesSnap = null;         /* 节点快照。两个坑（均已踩过）：
                                           1) 每次重收：trim() 会把上一轮打空的节点滤掉，
                                              重打丢行（macOS/26.6.1 消失）→ 只收一次；
                                           2) applyI18n 的 textContent= 会替换文本节点对象，
                                              若快照先于 applyI18n 建立，标签节点（应用版本/机型/语言）
                                              已脱离 DOM——在死节点上打字看不见，新节点直接满值显示
                                              （"有些直接显示出来"）。→ 发现失活节点立即重建快照 */
    function fxMinfoNodes() {
      if (fxMinfoNodesSnap && fxMinfoNodesSnap.some(function (n) { return !n.isConnected; })) {
        fxMinfoNodesSnap = null;                     /* 有死节点：重建 */
      }
      if (!fxMinfoNodesSnap) {
        var out = [], w = document.createTreeWalker($('.fx-minfo', fx), NodeFilter.SHOW_TEXT, null), n;
        while ((n = w.nextNode())) if (n.nodeValue.trim()) out.push(n);
        fxMinfoNodesSnap = out;
      }
      return fxMinfoNodesSnap;
    }
    function fxPrintUnpin() {
      var box = $('.fx-minfo', fx);
      if (box) box.style.minHeight = '';
    }
    function fxPrintRestore() {
      fxPrintTk++;                                   /* 取消进行中的打印 */
      fxMinfoNodes().forEach(function (node) { if (node.__fxFull != null) node.nodeValue = node.__fxFull; });
      fxPrintUnpin();
    }
    /* 打字机是否满文字（= 没有进行中的打印）：只有此时纸高才可作为基准 */
    function fxMinfoFullOK() {
      var nodes = fxMinfoNodes();
      for (var k = 0; k < nodes.length; k++) {
        var v = nodes[k].nodeValue, f = nodes[k].__fxFull;
        if (f != null && v !== f) return false;
      }
      return true;
    }
    function fxPrintMinfo() {
      var box = $('.fx-minfo', fx);
      var nodes = fxMinfoNodes();
      if (!box || !nodes.length) return;
      fxPrintTk++;
      var tk = fxPrintTk;
      /* 满高只信首次载入的量测：clip 脱流期间 absolute shrink-to-fit 塌宽（492→151px），
         文字换行会让 offsetHeight 量出 33px 之类的假值，钉上后纸面永久矮一截 */
      if (fxMinfoFullH == null) fxMinfoFullH = box.offsetHeight;
      box.style.minHeight = fxMinfoFullH + 'px';
      nodes.forEach(function (node) { node.__fxFull = node.nodeValue; node.nodeValue = ''; });
      var done = function () {
        if (tk !== fxPrintTk) return;
        fxPrintUnpin();
        fxOnBaseReady();                             /* 满文字时刻：可记/校准开态基准 */
      };
      if (REDUCED) { nodes.forEach(function (node) { node.nodeValue = node.__fxFull; }); fxPrintUnpin(); return; }
      var i = 0, j = 0;
      /* 逐字打字（视觉每字 14ms/行间 22ms ≈ 0.95s，所有者逐轮定档：
         0.44s 太快/2.4s 太慢/1.2s 折中后要求整体再加快一点）。音效密度
         对齐官版视频实测（6 声/45 字 ≈ 每 6 字一声，间隔 >100ms 可数清单击）：
         每字都响会让 10ms 短样本叠成连绵噪音，听感"打了无数字" */
      var tick = 0;
      (function step() {
        if (tk !== fxPrintTk) return;                /* 已被取消/替换 */
        var node = nodes[i];
        if (!node) { done(); return; }               /* 打完 */
        node.nodeValue = node.__fxFull.slice(0, ++j);
        if (++tick % 5 === 0) playFax('fax-print-' + (1 + Math.floor(Math.random() * 3)), 0.5);
        if (j >= node.__fxFull.length) { i++; j = 0; setTimeout(step, 16); }   /* 换行进给 */
        else setTimeout(step, 10);
      })();
    }

    function fxRef() {
      var s = '';
      for (var i = 0; i < 7; i++) s += String.fromCharCode(65 + Math.floor(Math.random() * 26));
      return 'R-' + s;
    }
    function fxFillReceipt() {
      var set = function (sel, v) { var el = $(sel, fx); if (el) el.textContent = v; };
      set('[data-fx-r-date]', fxNow().stamp);
      set('[data-fx-r-from]', fxEmail && fxEmail.value.trim() ? fxEmail.value.trim() : t('fx.anon'));
      set('[data-fx-r-type]', t(fxMode === 'bug' ? 'fx.catBug' : 'fx.catSug'));
      set('[data-fx-r-chars]', fxPad(fxChars(), 4));
      set('[data-fx-r-time]', '00\'' + fxPad(Math.max(0, Math.round((performance.now() - fxT0) / 1000)), 2));
      set('[data-fx-r-result]', t('fx.okDelivered'));
      set('[data-fx-r-ref]', fxRef());
    }

    function fxSend() {
      if (fxPhase !== 'ready') return;
      if (!fxChars()) {
        /* 空内容：报错 + fax-error，停留待命 */
        playFax('fax-error');
        if (fxStateZh) fxStateZh.textContent = t('fx.st.empty');
        setTimeout(function () { if (fxPhase === 'ready') fxShow('ready'); }, 1500);
        return;
      }
      fxT0 = performance.now();
      fx._pct = 10;
      /* 官方线性时间轴（音画交叉验证）：摘机170→拨号170→载波320→传送1820→切换390→回执650 */
      fxSetPhase('offhook');
      playFax('fax-offhook');
      setTimeout(function () {
        fxSetPhase('dialing');
        playFax('fax-dial-1', 1);   /* 官方：dial-1 立即 1 声（0x10006245c IMM case26） */
        /* 官方拨号序列（runTransmission 0x100062864 计数器查表 @0x10023a680=[26..31]）：
           计数器 1..5 → dial-2→3→4→5→6，与首声组成完整六音，间隔 160ms（const 池） */
        for (var di = 2; di <= 6; di++) {
          (function (n) {
            fxSfxTo(function () {
              if (fxPhase === 'dialing') playFax('fax-dial-' + n, 1);
            }, REDUCED ? 0 : 160 * (n - 1));
          })(di);
        }
        setTimeout(function () {
          fxSetPhase('connecting');
          playFax('fax-carrier', 1);   /* 载波握手音 0.70s 播一次（官版第二轮 19.9-20.35 实测仅 0.5s 强声区——连播 3 次是误读第一轮混音） */
          setTimeout(function () {
            fxSetPhase('sending');
            playFax('fax-send-key');
            var p0 = performance.now();
            var feedStep = 0;             /* 官方 feed-1/2/3 轮换计数（mod-3 查表） */
            var printStep = 0;            /* 官方 print-1/2/3 轮换计数（回执段） */
            /* 传送段持续走纸音：滚轮声循环（官版 SENDING 全程有音，间隔渐宽=纸加速） */
            /* 官方 rollSheet 同构步进：一个时钟（107ms/步）同时驱动【纸位移+滚轮声】
               ——音画物理同源，杜绝 CSS 合成器与 JS 定时器双时钟漂移 */
            if (fxPaper) { fxPaper.classList.remove('up'); fxPaper.style.transform = ''; }
            /* 主时钟=进度百分比（所有者定）：%=10→100，纸位移/滚轮声/LCD 全部由 k 驱动；
               k=1（100%）时由循环自己收尾——纸全进/声渐停/切送达同刻发生 */
            var sendDone = function () {
              if (fxPhase !== 'sending') return;
              fx._pct = 100;
              if (fxStateEn) fxStateEn.textContent = fxEnLine('sending');
              fxSfxClear();                     /* 最后一步声已响，收尾即静 */
              finishSend();                     /* → 已送达 → 回执 */
            };
            fxSfxTo(function tick() {
              if (fxPhase !== 'sending') { fxSfxClear(); return; }
              var k = Math.min(1, (performance.now() - p0) / 3200);
              var paper = $('.fx-paper', fx);
              if (paper) paper.style.transform = 'translateY(' + (-118 * k).toFixed(2) + '%)';
              fx._pct = Math.round(10 + k * 90);            /* 10% → 100% */
              if (fxStateEn) fxStateEn.textContent = fxEnLine('sending');
              if (k >= 1) { sendDone(); return; }           /* 100%=纸全进=切送达 */
              /* 官方传送段走纸音（feedSheet 0x10006415c mod-3 查表 @0x10023a6d0=[33,34,35]）：
                 feed-1→2→3 轮换，每步 1 声——与纸位移同一时钟 */
              playFax('fax-feed-' + (1 + feedStep % 3), 0.55 - k * 0.25);
              feedStep++;
              fxSfxTo(tick, k > 0.7 ? 107 + (k - 0.7) * 500 : 107);   /* 末段渐稀（官版尾音余韵） */
            }, 107);
            /* 收尾链由 sendDone（=百分比 100%）触发，不再用独立 3590ms 定时器
               （双时钟漂移=纸与声/进度错位的根源） */
            var finishSend = function () {
              fxSfxHold = true;            /* 编排链开始：相位切换不再清后续排定的音 */
              fxFillReceipt();
              fxSetPhase('sent');
              playFax('fax-ding', 1);   /* 已送达：叮一声（全二进制仅此 1 处 ding） */
              /* 官方回执段 print 循环（printReceipt 0x1000647f8 + FaxPrintPass 双层，
                 mod-3 表 [36,37,38]）：print-1→2→3 轮换，与回执滑入同刻起、落位同刻停 */
              playFax('fax-print-' + (1 + printStep++ % 3), 1);   /* 首声立即（与叮同刻） */
              fxSfxIv(function () {
                if (fxPhase !== 'sent' && fxPhase !== 'printed') { fxSfxClear(); return; }
                playFax('fax-print-' + (1 + printStep++ % 3), 1);
              }, 100);                    /* 官方 const 池 100ms（打印步进） */
              fxSfxTo(function () {
                fxSetPhase('printed');
                fxSfxTo(function () {
                  fxSfxClear();
                  fxSfxHold = false;        /* 编排链结束：恢复正常相位清音（hold 悬空=下轮相位切换不清音） */
                  playFax('fax-ding', 1);   /* 收尾：叮+落定 */
                  setTimeout(function () { playFax('fax-offhook', 1); }, 200);
                }, REDUCED ? 0 : 1050);
              }, REDUCED ? 0 : 300);
            };
          }, REDUCED ? 0 : 750);   /* 接通 750ms（官版常规节奏：视频为快剪） */
        }, REDUCED ? 0 : 950);   /* 拨号 950ms */
      }, REDUCED ? 0 : 650);     /* 摘机 650ms */
    }

    function fxTear() {
      if (fxPhase !== 'printed') return;
      fxSetPhase('tearing');
      /* 撕纸段官方真相（FaxReceiptTeeth 0x10005fd60 animatableData setter，IMM case43）：
         fax-tear 随回执齿条动画【每步 1 声】——非单响、非双响，随动画密度 */
      playFax('fax-tear', 1);   /* 首声立即 */
      fxSfxIv(function () {
        if (fxPhase !== 'tearing') { fxSfxClear(); return; }
        playFax('fax-tear', 1);
      }, 107);                 /* 与官方动画步进同钟（步进制 107ms） */
      setTimeout(function () {
        /* 换纸段官方真相：无 load 调用（全二进制零引用），吞纸声=feedSheet 复用
           （feed-1/2/3 mod-3 循环 @0x10023a6d0） */
        fxSerial += 1;
        if (fxNo) fxNo.textContent = 'NO.' + fxPad(fxSerial, 4);
        if (fxDateEl) fxDateEl.textContent = fxNow().date;
        if (fxText) fxText.value = '';
        fxCountUpdate();
        fxSetPhase('loading');
        var swallowStep = 0;           /* 换纸段独立 mod-3 计数（feedStep 在发送闭包内不可达） */
        playFax('fax-feed-' + (1 + swallowStep++ % 3), 1);   /* 吞纸首声立即 */
        fxSfxIv(function () {
          if (fxPhase !== 'loading') { fxSfxClear(); return; }
          playFax('fax-feed-' + (1 + swallowStep++ % 3), 1);
        }, 107);               /* 吞纸段与官方步进同钟（吞纸 1100ms，约 10 声） */
        setTimeout(function () {
          fxSfxClear();          /* 吞纸结束：停 feed 循环 */
          /* 新纸步进降下（官方慢慢出纸）：从 -118% 每 107ms 一步匀速回 0；
             官方结构：降下窗=新纸打印窗（视频指纹 24.9-26.9 打印声 2s ≈ 降下时长），
             FaxPrintPass 的 print mod-3 循环伴纸同响——纸是被打印声"顶"出来的 */
          var fresh0 = performance.now();
          var freshStep = 0;
          playFax('fax-print-' + (1 + freshStep++ % 3), 0.5);   /* 打印首声与纸出同刻 */
          fxSfxIv(function () {
            if (fxPhase !== 'loading') { fxSfxClear(); return; }
            playFax('fax-print-' + (1 + freshStep++ % 3), 0.5);
          }, 100);               /* 官方 const 池 100ms（打印步进），低音量（官版新纸打印弱于回执） */
          fxSfxTo(function down() {
            if (fxPhase !== 'loading') { fxSfxClear(); return; }
            var kk = Math.min(1, (performance.now() - fresh0) / 2000);
            if (fxPaper) fxPaper.style.transform = 'translateY(' + (-118 * (1 - kk)).toFixed(2) + '%)';
            fxSfxTo(down, 107);
          }, 107);
          fxSfxTo(function () {
            fxSfxClear();          /* 纸落位即停打印声（音画同刻收） */
            fxSetPhase('ready');
            if (fx.getAttribute('data-fx-attach') !== 'off') fxPrintMinfo();
          }, REDUCED ? 0 : 2000);   /* 新纸打印段 */
        }, REDUCED ? 0 : 1100);   /* 换纸吞纸 */
      }, REDUCED ? 0 : 310);       /* 撕纸（官方 0.31s） */
    }

    /* CLEAR：点按清纸；长按 1.5 秒演示占线 */
    var fxClearTimer = null, fxClearLong = false;
    if (fxClearBtn) {
      fxClearBtn.addEventListener('pointerdown', function () {
        if (fxPhase !== 'ready') return;
        fxClearLong = false;
        fxClearTimer = setTimeout(function () {
          fxClearLong = true;
          fxSetPhase('busy');
          playFax('fax-error');
          setTimeout(function () { if (fxPhase === 'busy') fxSetPhase('ready'); }, 1500);
        }, 1500);
      });
      var fxClearUp = function () { if (fxClearTimer) { clearTimeout(fxClearTimer); fxClearTimer = null; } };
      fxClearBtn.addEventListener('pointerup', fxClearUp);
      fxClearBtn.addEventListener('pointerleave', fxClearUp);
      fxClearBtn.addEventListener('click', function () {
        if (fxClearLong) { fxClearLong = false; return; }
        if (fxPhase !== 'ready') return;
        if (fxText) fxText.value = '';
        if (fxEmail) fxEmail.value = '';
        fxCountUpdate();
      });
    }

    if (fxSendBtn) fxSendBtn.addEventListener('click', fxSend);
    if (fxReceipt) fxReceipt.addEventListener('click', fxTear);

    fxOnLang = function () {
      fxShow(fxPhase);
      fxPaintStamp();
      if (fxMlang) fxMlang.textContent = t('fx.mlangValue');
      if (fxDateEl) fxDateEl.textContent = fxNow().date;
    };

    if (fxNo) fxNo.textContent = 'NO.' + fxPad(fxSerial, 4);
    if (fxDateEl) fxDateEl.textContent = fxNow().date;
    if (fxMlang) fxMlang.textContent = t('fx.mlangValue');
    fxPaintStamp();
    fxCountUpdate();
    fxSetPhase('ready');
    /* 首次打印推迟到脚本末尾 applyI18n() 之后：textContent= 会替换文本节点，
       先打印会让快照收进即将脱 DOM 的旧节点（标签行直接显示/丢行的根源） */
    if (fx.getAttribute('data-fx-attach') !== 'off') setTimeout(function () {
      if (fxPhase === 'ready' && fx.getAttribute('data-fx-attach') !== 'off') fxPrintMinfo();
    }, 0);
  }

  /* ———— FX-01 锚点滚动（GENERAL「打开…」滚动定位到传真机区块）———— */
  var fxOpenBtn = $('[data-feedback-open]');
  if (fxOpenBtn) {
    fxOpenBtn.addEventListener('click', function () {
      var fxBlock = document.getElementById('feedback-fx');
      if (!fxBlock) return;
      fxBlock.scrollIntoView({ behavior: REDUCED ? 'auto' : 'smooth' });
      play('keyPress');
    });
  }
  /* ══════════════════ 17. 石墨键帽：会响的键（§3 模式键同款声）══════════════════ */
  $$('.pt-keycap').forEach(function (k) {
    k.addEventListener('click', function () {
      play('keyPress');
    });
  });

  /* ══════════════════ 18. 整体缩放（§14）══════════════════ */
  var stageEl = $('#ptStage');
  var rigEl = $('#ptRig');
  function fitStage() {
    if (!stageEl || !rigEl) return;
    var s = Math.min(1, stageEl.clientWidth / 540);
    rigEl.style.setProperty('--pt-scale', s);
    document.documentElement.style.setProperty('--pt-scale', s);
    stageEl.style.setProperty('--pt-scale', s);
  }
  if (stageEl && typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(fitStage).observe(stageEl);
  } else if (stageEl) {
    window.addEventListener('resize', fitStage);
  }

  /* ══════════════════ 19. CJK 字距（§14）══════════════════ */
  function applyCjk() {
    $$('.pt-key span, .pt-drum i span').forEach(function (el) {
      var host = el.parentElement;
      host.classList.toggle('cjk', CJK_RE.test(el.textContent));
    });
  }

  /* ══════════════════ 20. 主题与语言切换（页面壳）══════════════════ */
  var themeBtn = $('#ptThemeBtn');
  function setTheme(next) {
    document.documentElement.setAttribute('data-theme', next);
    if (themeBtn) $('span', themeBtn).textContent = t(next === 'light' ? 'page.themeDark' : 'page.themeLight');
  }
  if (themeBtn) {
    themeBtn.addEventListener('click', function () {
      var cur = document.documentElement.getAttribute('data-theme');
      setTheme(cur === 'light' ? 'dark' : 'light');
    });
  }
  var langBtn = $('#ptLangBtn');
  if (langBtn) {
    langBtn.addEventListener('click', function () {
      LANG = LANG === 'zh' ? 'en' : 'zh';
      applyI18n();
      paint();
      applyCjk();
      fxOnLang();
      var cur = document.documentElement.getAttribute('data-theme') || 'light';
      if (themeBtn) $('span', themeBtn).textContent = t(cur === 'light' ? 'page.themeDark' : 'page.themeLight');
    });
  }

  /* ══════════════════ 21. 启动 ══════════════════ */
  buildOdo($('[data-odo="speed"]'), st.speed.toFixed(2));
  buildOdo($('[data-odo="undo"]'), st.undo.toFixed(1));
  var odoAwake0 = $('[data-odo="awake"]');
  if (odoAwake0) { buildOdo(odoAwake0, awakeText()); odoAwake0.dataset.text = awakeText(); }
  applyI18n();
  paint();
  applyCjk();
  fitStage();

})();
