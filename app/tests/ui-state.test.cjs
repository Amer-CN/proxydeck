'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const htmlPath = path.join(__dirname, '..', 'ui.html');
const html = fs.readFileSync(htmlPath, 'utf8');

function scriptById(id) {
  const match = html.match(new RegExp(`<script[^>]*id=["']${id}["'][^>]*>([\\s\\S]*?)<\\/script>`));
  assert.ok(match, `生产 HTML 必须包含脚本 #${id}`);
  return match[1];
}

function scriptContaining(marker) {
  const scripts = [...html.matchAll(/<script[^>]*>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
  const script = scripts.find((body) => body.includes(marker));
  assert.ok(script, `生产 HTML 必须包含 ${marker}`);
  return script;
}

function regionContext() {
  const window = {};
  const context = vm.createContext({ window, console, Promise, Object });
  vm.runInContext(scriptById('pt-region-state'), context);
  return { window, context };
}

function flush() {
  return new Promise((resolve) => setImmediate(resolve));
}

test('PTApp region context is immutable, generationed and notifies subscribers', () => {
  const { window } = regionContext();
  const app = window.PTApp;
  const seen = [];
  app.onRegionChange((pageId, current, previous) => seen.push({ pageId, current, previous }));

  const cn = app.setRegionContext('cb', 'codebuddy', 'cn', 8787);
  assert.deepEqual({ ...cn }, { pluginId: 'codebuddy', region: 'cn', port: 8787, generation: cn.generation });
  assert.ok(Object.isFrozen(cn));
  assert.ok(app.isCurrent(cn));

  const intl = app.setRegionContext('cb', 'codebuddy-intl', 'intl', 8789);
  assert.equal(app.isCurrent(cn), false);
  assert.equal(app.isCurrent(intl), true);
  assert.equal(app.regionContext('cb').port, 8789);
  assert.equal(seen.length, 2);
  assert.equal(seen[1].previous.port, 8787);

  app._advanceRegionGeneration();
  assert.equal(app.isCurrent(intl), false, '模式变化后旧页面上下文必须失效');
});

test('production WorkBuddy account script discards late CN response after INTL switch', async () => {
  const { window, context } = regionContext();
  const listeners = {};
  const elements = {
    cbAccounts: { innerHTML: 'CN-CACHE', addEventListener: (name, fn) => { listeners[name] = fn; } },
    cbAccCount: { textContent: '' },
    cbAccRescan: { disabled: false },
    cbRefresh: { addEventListener: (name, fn) => { listeners.refresh = fn; } },
    cbEndpoint: { textContent: 'http://127.0.0.1:8787/v1' },
  };
  const pending = [];
  const fetch = (url) => new Promise((resolve) => pending.push({ url, resolve }));
  Object.assign(context, {
    document: {
      readyState: 'complete',
      getElementById: (id) => elements[id] || null,
      querySelector: () => null,
    },
    fetch,
    setTimeout,
    clearTimeout,
    AbortSignal: { timeout: () => ({}) },
  });
  window.PTApp.onModeChange = () => {};
  vm.runInContext(scriptContaining('WorkBuddy 账号池区'), context);

  const cn = window.PTApp.setRegionContext('cb', 'codebuddy', 'cn', 8787);
  assert.equal(pending.length, 1, '区域上下文建立后自动拉取 CN');
  assert.equal(pending[0].url, 'http://127.0.0.1:8787/v1/accounts');
  const intl = window.PTApp.setRegionContext('cb', 'codebuddy-intl', 'intl', 8789);
  assert.equal(pending.length, 2, '区域订阅应自动拉取 INTL');
  assert.equal(pending[1].url, 'http://127.0.0.1:8789/v1/accounts');
  pending[0].resolve({ json: async () => ({ accounts: [{ uid: 'cn', nickname: 'CN-ACCOUNT' }] }) });
  await flush();
  await flush();
  assert.doesNotMatch(elements.cbAccounts.innerHTML, /CN-ACCOUNT/);
  assert.equal(elements.cbAccounts.innerHTML, 'CN-CACHE');
  assert.equal(window.PTApp.isCurrent(cn), false);
  assert.equal(window.PTApp.isCurrent(intl), true);

  pending[1].resolve({ json: async () => ({ accounts: [{ uid: 'intl', nickname: 'INTL-ACCOUNT' }] }) });
  await flush();
  await flush();
  assert.match(elements.cbAccounts.innerHTML, /INTL-ACCOUNT/);
  assert.doesNotMatch(elements.cbAccounts.innerHTML, /CN-ACCOUNT/);
  assert.ok(pending.every((item) => item.url.includes('/v1/accounts')), '区域切换不得发停服请求');
});

test('production Qoder region request rejects a late CN health response', async () => {
  const { window } = regionContext();
  const app = window.PTApp;
  const pending = [];
  const fetch = () => new Promise((resolve) => pending.push({ resolve }));
  app._setFetch(fetch);
  const cn = app.setRegionContext('qoder', 'qoder', 'cn', 8785);
  const request = app.regionRequest(cn, '/health');
  const intl = app.setRegionContext('qoder', 'qoder-intl', 'intl', 8791);
  assert.equal(pending.length, 1, 'CN 请求已发出');
  pending[0].resolve({ json: async () => ({ credits: { spent: 1 } }) });
  await assert.rejects(request, /stale/i);

  const current = app.regionRequest(intl, '/health');
  assert.equal(pending.length, 2, 'INTL 请求已发出');
  pending[1].resolve({ json: async () => ({ credits: { spent: 2 } }) });
  const response = await current;
  assert.deepEqual(await response.json(), { credits: { spent: 2 } });
});
