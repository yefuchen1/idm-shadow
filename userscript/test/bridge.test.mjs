// 油猴桥接脚本的单元测试。
//
//   node userscript/test/bridge.test.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, '..', 'idm-shadow-bridge.user.js'), 'utf8');

const gmValues = { settings: { bridgeUrl: 'http://127.0.0.1:16888', token: 'tk' } };
const requests = [];

globalThis.GM_getValue = (key, fallback) =>
  key in gmValues ? gmValues[key] : fallback;
globalThis.GM_setValue = (key, value) => {
  gmValues[key] = value;
};
globalThis.GM_registerMenuCommand = () => {};
globalThis.GM_xmlhttpRequest = (opts) => {
  requests.push(opts);
  if (opts.onload) {
    opts.onload({
      status: 200,
      responseText: JSON.stringify({ id: 'abc', status: 'queued' })
    });
  }
};

globalThis.document = {
  readyState: 'complete',
  documentElement: {},
  cookie: 'a=1',
  location: { href: 'https://example.com/page' },
  querySelectorAll: () => [],
  createElement: () => ({ style: {}, dataset: {}, addEventListener() {} }),
  addEventListener() {}
};
globalThis.window = {
  location: { href: 'https://example.com/page' },
  addEventListener() {},
  // 模拟油猴注入的 GM_download，脚本应当尝试接管它
  GM_download() {}
};
globalThis.MutationObserver = class {
  observe() {}
};

const factory = new Function(
  'GM_getValue',
  'GM_setValue',
  'GM_registerMenuCommand',
  'GM_xmlhttpRequest',
  'document',
  'window',
  'MutationObserver',
  'navigator',
  `${source}\nreturn { api: window.IDMShadow, GM_download: window.GM_download };`
);

function makeApi(gmRequest, win = globalThis.window) {
  return factory(
    globalThis.GM_getValue,
    globalThis.GM_setValue,
    globalThis.GM_registerMenuCommand,
    gmRequest,
    globalThis.document,
    win,
    globalThis.MutationObserver,
    { userAgent: 'Test UA/1.0' }
  );
}

const api = makeApi(globalThis.GM_xmlhttpRequest).api;

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

console.log('userscript/idm-shadow-bridge.user.js');

await test('download 会 POST 到 bridge 并带上 referer 与 cookie', async () => {
  requests.length = 0;
  const result = await api.download('https://example.com/v.mp4', {
    filename: 'v.mp4'
  });
  assert.equal(requests.length, 1);
  const req = requests[0];
  assert.equal(req.method, 'POST');
  assert.equal(req.url, 'http://127.0.0.1:16888/api/v1/tasks');
  assert.equal(req.headers['X-IDM-Shadow-Token'], 'tk');

  const body = JSON.parse(req.data);
  assert.equal(body.url, 'https://example.com/v.mp4');
  assert.equal(body.filename, 'v.mp4');
  assert.equal(body.referer, 'https://example.com/page');
  assert.equal(body.cookies, 'a=1');
  assert.equal(result.id, 'abc');
});

await test('应用连不上时回退到 idmshadow:// 协议', async () => {
  let navigated = null;
  const win = {
    location: {
      set href(v) {
        navigated = v;
      },
      get href() {
        return 'https://example.com/page';
      }
    },
    addEventListener() {}
  };

  const api2 = makeApi((opts) => {
    if (opts.onerror) opts.onerror();
  }, win).api;

  const result = await api2.download('https://example.com/f.mp4', {
    filename: 'f.mp4'
  });
  assert.equal(result.launched, true);
  assert.ok(
    navigated && navigated.startsWith('idmshadow://dl?'),
    `应当跳转到 idmshadow:// 协议，实际: ${navigated}`
  );
  assert.ok(navigated.includes('url='), '协议链接里应当带 url');
});

await test('alive() 能判断应用是否在跑', async () => {
  assert.equal(await api.alive(), true);
});

console.log('');
if (failures > 0) {
  console.error(`${failures} 个测试失败`);
  process.exit(1);
}
console.log('全部通过');
