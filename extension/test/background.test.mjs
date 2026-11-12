// 扩展后台逻辑的单元测试。
//
// background.js 跑在浏览器扩展环境里，没有 import/export，
// 所以这里用一个假的 chrome 全局把它加载起来，再验证行为。
//
//   node extension/test/background.test.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));

const calls = { fetch: [], downloads: [], notifications: [], tabs: [] };
let storage = {
  bridgeUrl: 'http://127.0.0.1:16888',
  token: 'test-token',
  mode: 'auto',
  autoLaunch: false,
  handled: {}
};
const listeners = {};

function record(list) {
  return (...args) => {
    calls[list].push(args);
  };
}

const chrome = {
  storage: {
    local: {
      get: async (defaults) => {
        const out = {};
        for (const [k, v] of Object.entries(defaults || {})) {
          out[k] = k in storage ? storage[k] : v;
        }
        return out;
      },
      set: async (patch) => {
        Object.assign(storage, patch);
      }
    }
  },
  cookies: {
    getAll: async () => [
      { name: 'session', value: 'abc123' },
      { name: 'theme', value: 'dark' }
    ]
  },
  downloads: {
    onCreated: { addListener: (fn) => (listeners.onCreated = fn) },
    cancel: record('downloads'),
    search: async () => []
  },
  notifications: {
    create: record('notifications'),
    onButtonClicked: { addListener: (fn) => (listeners.onButton = fn) },
    clear: () => {}
  },
  runtime: {
    onInstalled: { addListener: (fn) => (listeners.onInstalled = fn) },
    onMessage: { addListener: (fn) => (listeners.onMessage = fn) },
    sendMessage: () => {},
    lastError: null
  },
  contextMenus: {
    removeAll: (cb) => cb && cb(),
    create: () => {},
    onClicked: { addListener: (fn) => (listeners.onMenu = fn) }
  },
  alarms: {
    create: () => {},
    onAlarm: { addListener: (fn) => (listeners.onAlarm = fn) }
  },
  tabs: {
    create: record('tabs')
  },
  action: {
    setBadgeText: () => {},
    setBadgeBackgroundColor: () => {}
  }
};

globalThis.chrome = chrome;
globalThis.navigator = { userAgent: 'Test UA/1.0' };

const source = readFileSync(join(here, '..', 'background.js'), 'utf8');
const factory = new Function(
  'chrome',
  'fetch',
  `${source}\nreturn { capture, settings, apiUrl, createTask, cookieHeader, syncWithApp, effectiveMode };`
);

const fetchCalls = [];
let settingsResponse = { default_download_tool: false };
const fakeFetch = async (url, options = {}) => {
  fetchCalls.push({ url, options });
  return {
    ok: true,
    status: 200,
    json: async () =>
      url.endsWith('/api/v1/settings')
        ? settingsResponse
        : { ok: true, tasks: [] },
    text: async () => ''
  };
};

const api = factory(chrome, fakeFetch);

let failures = 0;

async function test(name, fn) {
  try {
    await fn();
    console.log(`  ok  ${name}`);
  } catch (err) {
    failures += 1;
    console.error(`FAIL  ${name}`);
    console.error(`      ${err.message}`);
  }
}

console.log('extension/background.js');

await test('捕获下载时 POST 到 bridge，附带 Cookie 与 UA', async () => {
  fetchCalls.length = 0;
  await api.capture({
    id: 42,
    url: 'https://example.com/file.mp4',
    filename: 'file.mp4',
    referrer: 'https://example.com/page'
  });

  const posts = fetchCalls.filter(
    (f) => f.options.method === 'POST' && f.url.endsWith('/api/v1/tasks')
  );
  assert.equal(posts.length, 1, '应当只投递一次任务');
  const req = posts[0];
  assert.equal(req.url, 'http://127.0.0.1:16888/api/v1/tasks');
  assert.equal(req.options.method, 'POST');
  assert.equal(req.options.headers['X-IDM-Shadow-Token'], 'test-token');

  const body = JSON.parse(req.options.body);
  assert.equal(body.url, 'https://example.com/file.mp4');
  assert.equal(body.filename, 'file.mp4');
  assert.equal(body.referer, 'https://example.com/page');
  assert.equal(body.cookies, 'session=abc123; theme=dark');
  assert.equal(body.autostart, true);
});

await test('内容脚本传来的 referer 也能被识别', async () => {
  fetchCalls.length = 0;
  await api.capture({
    id: 43,
    url: 'https://example.com/b.mp4',
    filename: 'b.mp4',
    referer: 'https://example.com/other'
  });
  const posts = fetchCalls.filter(
    (f) => f.options.method === 'POST' && f.url.endsWith('/api/v1/tasks')
  );
  const body = JSON.parse(posts[0].options.body);
  assert.equal(body.referer, 'https://example.com/other');
});

await test('blob: 与 data: URL 不接管', async () => {
  for (const url of ['blob:http://x/123', 'data:text/plain,hi', 'chrome-extension://abc/x']) {
    fetchCalls.length = 0;
    const ok = await api.capture({ id: 1, url });
    assert.equal(ok, false, `${url} 不该被接管`);
    assert.equal(fetchCalls.length, 0, `${url} 不该发请求`);
  }
});

await test('接管成功后取消浏览器下载', async () => {
  calls.downloads.length = 0;
  fetchCalls.length = 0;
  await listeners.onCreated({
    id: 7,
    url: 'https://example.com/x.zip',
    filename: 'x.zip',
    referer: 'https://example.com/'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 1, '应当取消浏览器下载');
  assert.equal(calls.downloads[0][0], 7);
});

await test('off 模式不接管', async () => {
  storage.mode = 'off';
  calls.downloads.length = 0;
  fetchCalls.length = 0;
  await listeners.onCreated({
    id: 8,
    url: 'https://example.com/y.zip',
    filename: 'y.zip'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 0, 'off 模式不该取消下载');
  assert.equal(fetchCalls.length, 0, 'off 模式不该发请求');
  storage.mode = 'auto';
});

await test('ask 模式弹通知而不是直接取消', async () => {
  storage.mode = 'ask';
  calls.notifications.length = 0;
  calls.downloads.length = 0;
  await listeners.onCreated({
    id: 9,
    url: 'https://example.com/z.zip',
    filename: 'z.zip'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.notifications.length, 1, '应当弹通知');
  assert.equal(calls.notifications[0][0], 'ask-9');
  assert.equal(calls.downloads.length, 0, 'ask 模式不该直接取消');
  storage.mode = 'auto';
});

await test('同一个下载不会重复接管', async () => {
  storage.mode = 'auto';
  fetchCalls.length = 0;
  await listeners.onCreated({
    id: 11,
    url: 'https://example.com/w.zip',
    filename: 'w.zip'
  });
  await new Promise((r) => setTimeout(r, 50));
  const afterFirst = fetchCalls.length;

  await listeners.onCreated({
    id: 11,
    url: 'https://example.com/w.zip',
    filename: 'w.zip'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(fetchCalls.length, afterFirst, '同一个 id 不该重复投递');
});

await test('跟随应用模式：应用说是默认下载工具就自动接管', async () => {
  storage.mode = 'follow-app';
  settingsResponse = { default_download_tool: true };

  await api.syncWithApp();
  assert.equal(api.effectiveMode(await api.settings()), 'auto');

  calls.downloads.length = 0;
  await listeners.onCreated({
    id: 21,
    url: 'https://example.com/follow.bin',
    filename: 'follow.bin'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 1, '默认下载工具时应当自动接管');
});

await test('跟随应用模式：应用说不是就不自动接管', async () => {
  storage.mode = 'follow-app';
  settingsResponse = { default_download_tool: false };

  await api.syncWithApp();
  assert.equal(api.effectiveMode(await api.settings()), 'ask');

  calls.downloads.length = 0;
  calls.notifications.length = 0;
  await listeners.onCreated({
    id: 22,
    url: 'https://example.com/follow2.bin',
    filename: 'follow2.bin'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 0, '不是默认下载工具时不该取消下载');
  assert.equal(calls.notifications.length, 1, '应该弹提示让用户确认');

  storage.mode = 'auto';
});

console.log('');
if (failures > 0) {
  console.error(`${failures} 个测试失败`);
  process.exit(1);
}
console.log('全部通过');
