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

const calls = {
  fetch: [],
  downloads: [],
  erase: [],
  notifications: [],
  tabs: [],
  reload: []
};
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
    erase: record('erase'),
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
    // 扩展从这儿知道自己跑的是哪一版。
    //
    // 读真实的 manifest.json——写死版本号的话，每次给扩展升版
    // 都会漏改这里，变成一条莫名其妙的失败。
    getManifest: () => ({
      version: JSON.parse(
        readFileSync(join(here, '..', 'manifest.json'), 'utf8')
      ).version
    }),
    // 自动更新用：把自己重载一遍。
    reload: () => {
      calls.reload.push(true);
    },
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
    create: record('tabs'),
    query: async () => [],
    onActivated: { addListener: (fn) => (listeners.onActivated = fn) }
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
  `${source}\nreturn { capture, settings, apiUrl, createTask, cookieHeader, syncWithApp, effectiveMode, autoConfigure, discoverApp, basename, looksLikeImage, isLocalFile, shouldSkip, shouldSkipImage, isRestoredDownload, hasUserGesture, noteGesture, recentGestures, dropRestored, reloadIfOutdated, compareVersions, extensionVersion };`
);

const fetchCalls = [];
let settingsResponse = { default_download_tool: false };
// 引导接口的响应，null 表示"这个端口没有应用"
let bootstrapResponse = { port: 16888, token: 'auto-found-token' };
// 按端口配置引导接口的响应。null = 整个引导接口都不通。
let bootstrapResponsesByPort = null;
// 让接下来的 N 次任务投递返回 401，用来模拟 token 过期/应用换端口。
let rejectPosts = 0;
const fakeFetch = async (url, options = {}) => {
  fetchCalls.push({ url, options });
  if (url.endsWith('/api/v1/bootstrap')) {
    if (!bootstrapResponse) {
      throw new Error('连接被拒绝');
    }
    // 每个端口给各自的 token——真实的机器上跑着好几个实例时，
    // 每个实例的 token 都不一样。
    const port = Number(new URL(url).port);
    if (bootstrapResponsesByPort) {
      const forPort = bootstrapResponsesByPort[port];
      if (!forPort) {
        throw new Error('连接被拒绝');
      }
      return {
        ok: true,
        status: 200,
        json: async () => forPort,
        text: async () => ''
      };
    }
    if (port && port !== 16888) {
      return {
        ok: true,
        status: 200,
        json: async () => ({ port, token: `token-for-${port}` }),
        text: async () => ''
      };
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

await test('没有后缀的 NAS 预览图也会被跳过', async () => {
  // 事故里漏掉的那一条：路径叫 capture_image，没有 .jpg，
  // 文件名也只有 capture_image。只看后缀会当成正式下载。
  const nas = 'http://192.168.2.39/api/capture_image?source=0';
  assert.equal(api.looksLikeImage(nas), true);
  assert.equal(api.looksLikeImage('capture_image'), true);
  assert.equal(await api.shouldSkip({ url: nas, filename: 'capture_image' }), true);

  // 服务器给了 image/* 也算。
  assert.equal(
    await api.shouldSkip({ url: 'https://x.com/api/raw', mime: 'image/jpeg' }),
    true
  );

  // 主机名里带 images 不代表资源是图片。
  assert.equal(api.looksLikeImage('https://images.example.com/file.zip'), false);
});

await test('浏览器恢复的旧下载不接管，0 字节记录会被清掉', async () => {
  // 复现 2026-11-19 20:12 的事故：浏览器重启时把几天前没下完的
  // 条目重新创建出来，startTime 还是旧的。它不是用户此刻发起的。
  api.recentGestures.length = 0;
  calls.downloads.length = 0;
  calls.erase.length = 0;
  fetchCalls.length = 0;

  const old = new Date(Date.now() - 4 * 24 * 3600 * 1000).toISOString();
  await listeners.onCreated({
    id: 7001,
    url: 'https://dl.kookapp.cn/assets/release/html_pc/kook_3321/Kook_PC_Setup.exe',
    filename: 'Kook_PC_Setup.exe',
    referrer: 'https://www.kookapp.cn/',
    startTime: old,
    bytesReceived: 0,
    state: 'in_progress'
  });
  await new Promise((r) => setTimeout(r, 50));

  assert.equal(
    fetchCalls.filter((f) => f.url.endsWith('/api/v1/tasks')).length,
    0,
    '恢复的旧下载不该变成任务'
  );
  assert.equal(calls.downloads.length, 1, '应当取消浏览器恢复的下载');
  assert.equal(calls.downloads[0][0], 7001);
  assert.equal(calls.erase.length, 1, '应当清掉 0 字节的历史记录，避免下次启动再复活');
  assert.equal(calls.erase[0][0].id, 7001);
});

await test('已经下了部分字节的恢复下载交给浏览器，不取消', async () => {
  api.recentGestures.length = 0;
  calls.downloads.length = 0;
  calls.erase.length = 0;
  fetchCalls.length = 0;

  const old = new Date(Date.now() - 4 * 24 * 3600 * 1000).toISOString();
  await listeners.onCreated({
    id: 7002,
    url: 'https://example.com/big.bin',
    filename: 'big.bin',
    referrer: 'https://example.com/',
    startTime: old,
    bytesReceived: 1024,
    state: 'in_progress'
  });
  await new Promise((r) => setTimeout(r, 50));

  assert.equal(
    fetchCalls.filter((f) => f.url.endsWith('/api/v1/tasks')).length,
    0
  );
  assert.equal(calls.downloads.length, 0, '已经下了部分字节的别动');
  assert.equal(calls.erase.length, 0);
});

await test('页面自动发起的下载不接管、不取消', async () => {
  // 没有任何用户手势，referrer 指向页面。这种是页面脚本自己下载，
  // 不是用户点的。扩展不该投任务，也不该打断浏览器的下载。
  api.recentGestures.length = 0;
  calls.downloads.length = 0;
  calls.erase.length = 0;
  fetchCalls.length = 0;

  await listeners.onCreated({
    id: 8001,
    url: 'https://ads.example.com/promo.exe',
    filename: 'promo.exe',
    referrer: 'https://ads.example.com/landing',
    startTime: new Date().toISOString(),
    bytesReceived: 0,
    state: 'in_progress'
  });
  await new Promise((r) => setTimeout(r, 50));

  assert.equal(
    fetchCalls.filter((f) => f.url.endsWith('/api/v1/tasks')).length,
    0,
    '页面自动下载不该变成任务'
  );
  assert.equal(calls.downloads.length, 0, '也不该取消浏览器的下载');
  assert.equal(calls.erase.length, 0);
});

await test('用户点击后的下载照常接管，并标记来源', async () => {
  api.recentGestures.length = 0;
  api.noteGesture('https://example.com/page', 'click');
  calls.downloads.length = 0;
  fetchCalls.length = 0;

  await listeners.onCreated({
    id: 8002,
    url: 'https://example.com/setup.exe',
    filename: 'setup.exe',
    referrer: 'https://example.com/page',
    startTime: new Date().toISOString(),
    bytesReceived: 0,
    state: 'in_progress'
  });
  await new Promise((r) => setTimeout(r, 50));

  const posted = fetchCalls.find(
    (f) => f.options.method === 'POST' && f.url.endsWith('/api/v1/tasks')
  );
  assert.ok(posted, '用户点击的下载应当被接管');
  const body = JSON.parse(posted.options.body);
  assert.equal(body.source, 'user');
  assert.equal(body.needs_confirm, true);
  assert.equal(calls.downloads.length, 1, '接管成功后取消浏览器下载');
});

await test('没有 user 标记的 capture-url 不接管', async () => {
  // 老内容脚本不会带 source。放进来就等于分不清点击和页面自动下载。
  api.recentGestures.length = 0;
  fetchCalls.length = 0;

  const response = await new Promise((resolve) => {
    listeners.onMessage(
      {
        type: 'capture-url',
        url: 'https://example.com/auto.bin',
        referer: 'https://example.com/'
      },
      { tab: { id: 1 }, url: 'https://example.com/' },
      resolve
    );
  });
  assert.equal(response.ok, false);
  assert.equal(
    fetchCalls.filter((f) => f.options.method === 'POST').length,
    0
  );
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
  api.recentGestures.length = 0;
  api.noteGesture('https://example.com/', 'click');
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

await test('请求会带上扩展自己的版本号', async () => {
  // 应用要靠这个版本号认出"浏览器跑着旧代码的扩展"。
  //
  // 浏览器把已解压扩展的注册信息缓存住了：磁盘上的文件换了新版，
  // 浏览器照样跑旧代码。而旧代码压根不发 X-IDM-Shadow-Client 头，
  // 应用那边就只看到"扩展没连上"——和"扩展没装"没法区分。
  // 带上版本号之后，应用就能直接告诉用户该去点「重新加载」。
  fetchCalls.length = 0;
  await api.capture({
    id: 91,
    url: 'https://example.com/versioned.bin',
    filename: 'versioned.bin'
  });

  const posted = fetchCalls.find(
    (c) => c.options.method === 'POST' && c.url.endsWith('/api/v1/tasks')
  );
  assert.ok(posted, '应该把下载投给了应用');
  assert.equal(posted.options.headers['X-IDM-Shadow-Client'], 'extension');
  assert.equal(
    posted.options.headers['X-IDM-Shadow-Version'],
    api.extensionVersion(),
    '请求头里带的版本应该就是 manifest 里的版本'
  );
});

await test('版本号比较：只认"我更旧"', async () => {
  assert.equal(api.compareVersions('0.1.4', '0.1.5'), -1);
  assert.equal(api.compareVersions('0.1.5', '0.1.5'), 0);
  assert.equal(api.compareVersions('0.1.6', '0.1.5'), 1);
  assert.equal(api.compareVersions('0.2.0', '0.1.9'), 1);
  assert.equal(api.compareVersions('0.1.4', '0.1.5.1'), -1);
});

await test('比应用旧就自己重载一次', async () => {
  // 浏览器把已解压扩展的注册信息缓存住了：文件换了新版，浏览器照样跑
  // 旧代码。以前只能靠用户知道要去扩展管理页点「重新加载」——
  // 没人会主动做这个动作，症状又只是"点了下载没反应"。
  // 扩展自己发现自己落后，就该把自己重载。
  calls.reload.length = 0;
  storage.lastReloadAttempt = 0;

  const mine = api.extensionVersion();
  await api.reloadIfOutdated(mine);
  assert.equal(calls.reload.length, 0, '版本一样不该重载');

  await api.reloadIfOutdated('99.0.0');
  assert.equal(calls.reload.length, 1, '比应用旧就该重载');
});

await test('不会把自己降级，也不会重载成死循环', async () => {
  calls.reload.length = 0;
  storage.lastReloadAttempt = 0;

  // 用户自己装了个更新的扩展——不能把它降回去。
  await api.reloadIfOutdated('0.0.1');
  assert.equal(calls.reload.length, 0, '自己比应用新就不该动');

  // 重载了也没生效时（文件还是旧的），十分钟内不再重载。
  // 不加这个限制就是个不停重启扩展的死循环，比不更新还糟糕。
  calls.reload.length = 0;
  storage.lastReloadAttempt = Date.now();
  await api.reloadIfOutdated('99.0.0');
  assert.equal(calls.reload.length, 0, '十分钟内不该重复重载');
});

await test('自动配置：优先用已经配好的那个实例', async () => {
  // 机器上可能同时跑着好几个 IDM Shadow（开发、测试都会各起一个）。
  // 以前只按端口从小到大扫，扫到的第一个是谁全看运气——
  // 用户明明配的是另一个实例，扩展却连上了别人家那个，
  // 任务就投到不该投的地方去了。
  storage.bridgeUrl = 'http://127.0.0.1:16890';
  storage.token = 'configured-token';
  bootstrapResponsesByPort = {
    16888: { port: 16888, token: 'token-for-16888' },
    16890: { port: 16890, token: 'configured-token' }
  };

  const found = await api.discoverApp();
  assert.ok(found, '应该找到应用');
  assert.equal(
    found.bridgeUrl,
    'http://127.0.0.1:16890',
    '已经配好的地址应该优先，不要被扫端口的结果覆盖'
  );
  assert.equal(found.token, 'configured-token');
  bootstrapResponsesByPort = null;
});

await test('自动配置：配好的那个没了才退回扫端口', async () => {
  // 配好的实例已经关了（应用重装、端口被占），这时才该去找新的。
  storage.bridgeUrl = 'http://127.0.0.1:16899';
  storage.token = 'configured-token';
  bootstrapResponsesByPort = {
    16888: { port: 16888, token: 'token-for-16888' },
    16890: { port: 16890, token: 'token-for-16890' }
  };

  const found = await api.discoverApp();
  assert.ok(found, '应该找到别的实例');
  assert.equal(found.bridgeUrl, 'http://127.0.0.1:16888');
  assert.equal(found.token, 'token-for-16888');

  storage.bridgeUrl = 'http://127.0.0.1:16888';
  bootstrapResponsesByPort = null;
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
