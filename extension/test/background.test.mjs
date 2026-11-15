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
    onDeterminingFilename: {
      addListener: (fn) => (listeners.onDeterminingFilename = fn)
    },
    onChanged: { addListener: (fn) => (listeners.onChanged = fn) },
    cancel: record('downloads'),
    search: async () => []
  },
  notifications: {
    // 真实浏览器会校验通知的必填字段（type/iconUrl/title/message），
    // 少一个就抛 "Some of the required properties are missing"。
    // mock 也照着校验，免得这种错误等到实机才发现。
    create: (...args) => {
      const [, opts] = args;
      const missing = ['type', 'iconUrl', 'title', 'message'].filter(
        (k) => !opts || !opts[k]
      );
      if (missing.length) {
        throw new Error(
          `Some of the required properties are missing: ${missing.join(', ')}`
        );
      }
      calls.notifications.push(args);
    },
    onButtonClicked: { addListener: (fn) => (listeners.onButton = fn) },
    clear: () => {}
  },
  runtime: {
    onInstalled: { addListener: (fn) => (listeners.onInstalled = fn) },
    onStartup: { addListener: (fn) => (listeners.onStartup = fn) },
    onMessage: { addListener: (fn) => (listeners.onMessage = fn) },
    getURL: (p) => `chrome-extension://test-id/${p}`,
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
  `${source}\nreturn { capture, settings, apiUrl, createTask, cookieHeader, syncWithApp, effectiveMode, autoConfigure, discoverApp, basename, looksLikeImage, isLocalFile, shouldSkip };`
);

const fetchCalls = [];
let settingsResponse = { default_download_tool: false };
// 引导接口的响应，null 表示"这个端口没有应用"
let bootstrapResponse = { port: 16888, token: 'auto-found-token' };
// 让接下来的 N 次任务投递返回 401，用来模拟 token 过期/应用换端口。
let rejectPosts = 0;
const fakeFetch = async (url, options = {}) => {
  fetchCalls.push({ url, options });
  if (url.endsWith('/api/v1/bootstrap')) {
    if (!bootstrapResponse) {
      throw new Error('连接被拒绝');
    }
    return {
      ok: true,
      status: 200,
      json: async () => bootstrapResponse,
      text: async () => ''
    };
  }
  if (url.endsWith('/api/v1/tasks') && options.method === 'POST' && rejectPosts > 0) {
    rejectPosts -= 1;
    return {
      ok: false,
      status: 401,
      json: async () => ({ error: { message: 'token 不正确' } }),
      text: async () => ''
    };
  }
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
  // 走 IDM 式流程：应用会弹保存位置确认框
  assert.equal(body.needs_confirm, true);
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

await test('文件名只取 basename，完整路径被丢掉', async () => {
  // Chrome 的 downloads.onCreated 给的是完整路径。
  // 直接用的话文件名会变成 C__Users_ROG_Desktop_x.rar 这种压平样子，
  // 后面转存的时候路径也对不上。
  assert.equal(api.basename('C:\\Users\\ROG\\Desktop\\x.rar'), 'x.rar');
  assert.equal(api.basename('d:\\Users\\ROG\\Downloads\\字体_宋体.rar'), '字体_宋体.rar');
  assert.equal(api.basename('/tmp/foo/bar.zip'), 'bar.zip');
  assert.equal(api.basename('plain.txt'), 'plain.txt');
  assert.equal(api.basename(''), null);

  fetchCalls.length = 0;
  await api.capture({
    id: 44,
    url: 'https://example.com/path.rar',
    filename: 'C:\\Users\\ROG\\Desktop\\real.rar'
  });
  const posts = fetchCalls.filter(
    (f) => f.options.method === 'POST' && f.url.endsWith('/api/v1/tasks')
  );
  const body = JSON.parse(posts[0].options.body);
  assert.equal(body.filename, 'real.rar', '投给应用的应该是纯文件名');
});

await test('图片预览默认不接管', async () => {
  assert.equal(api.looksLikeImage('https://x.com/a/b.png'), true);
  assert.equal(api.looksLikeImage('https://x.com/a/b.PNG?width=200'), true);
  assert.equal(api.looksLikeImage('https://x.com/a/b.zip'), false);

  assert.equal(await api.shouldSkip({ url: 'https://x.com/p/1.jpg' }), true);
  assert.equal(
    await api.shouldSkip({ url: 'https://x.com/p/1.jpg', filename: 'C:\\Users\\ROG\\Desktop\\1.jpg' }),
    true
  );
  assert.equal(await api.shouldSkip({ url: 'https://x.com/p/a.zip' }), false);
});

await test('图片预览既不接管也不取消浏览器下载', async () => {
  calls.downloads.length = 0;
  calls.notifications.length = 0;
  fetchCalls.length = 0;
  await listeners.onCreated({
    id: 41,
    url: 'https://images-wm.liblib.cloud/preview/abc.jpg',
    filename: 'C:\\Users\\ROG\\Desktop\\abc.jpg'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 0, '不该取消浏览器的下载');
  assert.equal(calls.notifications.length, 0, '也不该弹通知');
  assert.equal(
    fetchCalls.filter((f) => f.url.endsWith('/api/v1/tasks')).length,
    0,
    '不该往应用里投任务'
  );
});

await test('关掉过滤之后图片照常接管', async () => {
  storage.skipImages = false;
  assert.equal(await api.shouldSkip({ url: 'https://x.com/p/1.jpg' }), false);
  storage.skipImages = true;
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

await test('旧设置 ask 也直接接管，不再弹浏览器通知气泡', async () => {
  // 用户不想在浏览器右下角看到"用 IDM Shadow 下载？"那个气泡，
  // 想直接看到 IDM Shadow 自己的下载对话框。
  // 所以老版本存下来的 'ask' 现在按"直接接管"处理。
  storage.mode = 'ask';
  calls.notifications.length = 0;
  calls.downloads.length = 0;
  await listeners.onCreated({
    id: 9,
    url: 'https://example.com/z.zip',
    filename: 'z.zip'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.notifications.length, 0, '不该再弹浏览器通知气泡');
  assert.equal(calls.downloads.length, 1, '应当直接取消浏览器下载');
  assert.equal(calls.downloads[0][0], 9);
  storage.mode = 'auto';
});

await test('接管失败就安静放手，不弹气泡也不取消下载', async () => {
  // blob: 地址引擎取不到，接管不了。
  // 这时候不能让用户看到"用 IDM Shadow 下载？"那种莫名其妙的提问——
  // 那个气泡既不取消下载，点不点结果一样。让它安安静静交给浏览器。
  storage.mode = 'auto';
  calls.notifications.length = 0;
  calls.downloads.length = 0;
  await listeners.onCreated({
    id: 31,
    url: 'blob:http://example.com/1234',
    filename: 'blob.bin'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.notifications.length, 0, '不该弹任何气泡');
  assert.equal(calls.downloads.length, 0, '没接管成功就不该取消浏览器下载');
});

await test('本地文件不接管', async () => {
  // 右键另存的图片、file:// 打开的本地资源——已经在用户自己盘上了，
  // 没有"下载"这回事，拦下来只会让人莫名其妙。
  assert.equal(api.isLocalFile({ url: 'file:///D:/work/a.jpg' }), true);
  assert.equal(
    api.isLocalFile({ url: 'https://x.com/a', filename: 'D:\\work\\a.jpg' }),
    true
  );
  assert.equal(
    api.isLocalFile({ url: 'https://x.com/a.zip', filename: 'a.zip' }),
    false
  );

  calls.downloads.length = 0;
  calls.notifications.length = 0;
  fetchCalls.length = 0;
  await listeners.onCreated({
    id: 55,
    url: 'file:///D:/work/%E5%8F%82%E8%80%83/2.jpg',
    filename: 'D:\\work\\参考\\2.jpg'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 0, '不该取消浏览器下载');
  assert.equal(calls.notifications.length, 0, '不该弹气泡');
  assert.equal(
    fetchCalls.filter((f) => f.url.endsWith('/api/v1/tasks')).length,
    0,
    '不该往应用里投任务'
  );
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
  // 应用里没勾"设为默认下载工具"，就完全不接管，
  // 下载老老实实交给浏览器——也不弹那个通知气泡。
  assert.equal(api.effectiveMode(await api.settings()), 'off');

  calls.downloads.length = 0;
  calls.notifications.length = 0;
  await listeners.onCreated({
    id: 22,
    url: 'https://example.com/follow2.bin',
    filename: 'follow2.bin'
  });
  await new Promise((r) => setTimeout(r, 50));
  assert.equal(calls.downloads.length, 0, '不是默认下载工具时不该取消下载');
  assert.equal(calls.notifications.length, 0, '不接管时也不该弹通知气泡');

  storage.mode = 'auto';
});

await test('自动配置：从引导接口拿到地址和 token', async () => {
  fetchCalls.length = 0;
  delete storage.token;
  delete storage.bridgeUrl;

  const found = await api.discoverApp();
  assert.ok(found, '应该能找到应用');
  assert.equal(found.bridgeUrl, 'http://127.0.0.1:16888');
  assert.equal(found.token, 'auto-found-token');
  assert.ok(
    fetchCalls.some((c) => c.url.endsWith('/api/v1/bootstrap')),
    '应该请求了引导接口'
  );
});

await test('自动配置：写进 storage，下游就能直接用', async () => {
  delete storage.token;
  delete storage.bridgeUrl;
  storage.mode = 'auto';

  await api.autoConfigure(true);

  assert.equal(storage.bridgeUrl, 'http://127.0.0.1:16888');
  assert.equal(storage.token, 'auto-found-token');

  // 配好之后，捕获下载就该直接投给应用
  fetchCalls.length = 0;
  calls.downloads.length = 0;
  await listeners.onCreated({
    id: 77,
    url: 'https://example.com/auto.bin',
    filename: 'auto.bin'
  });
  await new Promise((r) => setTimeout(r, 50));
  const posted = fetchCalls.find((c) => c.url.endsWith('/api/v1/tasks'));
  assert.ok(posted, '应该把下载投给了应用');
  const body = JSON.parse(posted.options.body);
  assert.equal(body.needs_confirm, true, '应该走 IDM 式确认流程');
});

await test('自动配置：应用没在跑时不乱改已有配置', async () => {
  bootstrapResponse = null;
  storage.bridgeUrl = 'http://127.0.0.1:16888';
  storage.token = 'manual-token';

  const found = await api.discoverApp();
  assert.equal(found, null, '找不到应用应该返回 null');

  await api.autoConfigure(true);
  assert.equal(storage.token, 'manual-token', '不该把已有 token 冲掉');

  bootstrapResponse = { port: 16888, token: 'auto-found-token' };
});

await test('token 过期时自动重新配置，再投一次就成功', async () => {
  // 应用重装/换端口之后，扩展里存的还是旧 token。
  // 以前只认"token 为空才自动配置"，于是一直 401，而且 401 之后
  // 还会把浏览器下载取消掉——用户的文件就这么没了。
  storage.bridgeUrl = 'http://127.0.0.1:16888';
  storage.token = 'stale-token';
  storage.mode = 'auto';
  storage.handled = {};
  fetchCalls.length = 0;
  rejectPosts = 1;

  const ok = await api.capture({ id: 901, url: 'https://example.com/stale.bin' });

  assert.equal(ok, true, '重新配置之后应当投递成功');
  assert.equal(storage.token, 'auto-found-token', '应当换成引导接口给的新 token');
  const posts = fetchCalls.filter(
    (f) => f.options.method === 'POST' && f.url.endsWith('/api/v1/tasks')
  );
  assert.equal(posts.length, 2, '应当先失败一次、重新配置后再投一次');
  assert.equal(posts[1].options.headers['X-IDM-Shadow-Token'], 'auto-found-token');
});

await test('应用接不住任务时不取消浏览器下载', async () => {
  storage.token = '';
  storage.mode = 'auto';
  storage.handled = {};
  bootstrapResponse = null;
  rejectPosts = 99;
  calls.downloads.length = 0;

  await listeners.onCreated({
    id: 902,
    url: 'https://example.com/lost.bin',
    filename: 'lost.bin'
  });

  // 轮询 10 次 × 800ms，等它放弃
  await new Promise((r) => setTimeout(r, 9000));
  assert.equal(
    calls.downloads.length,
    0,
    '投递没成功就不该取消浏览器下载，否则文件会凭空消失'
  );

  rejectPosts = 0;
  bootstrapResponse = { port: 16888, token: 'auto-found-token' };
});

console.log('');
if (failures > 0) {
  console.error(`${failures} 个测试失败`);
  process.exit(1);
}
console.log('全部通过');
