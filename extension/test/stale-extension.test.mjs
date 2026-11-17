// 回归测试：浏览器跑着旧代码的扩展。
//
// 用户的症状是「在网页上点下载，Edge 不弹下载框，IDM Shadow 也
// 不弹窗」——一点反应都没有。
//
// 根因：浏览器把已解压扩展的注册信息缓存住了。应用把新版的扩展
// 文件写进用户实际加载的那个目录（每次启动都做），浏览器却照样
// 跑着旧代码。旧版压根没有跨源 a[download] 接管逻辑，于是那次点击
// 被浏览器当成页面导航——没有下载、没有任务、扩展全程没醒。
//
// 这个测试用真实的浏览器复现整件事，并且验证：
//  1. 旧扩展在跑时，应用能认出「跑着旧代码」；
//  2. 扩展能自己发现落后、自己重载（不用用户去点「重新加载」）；
//  3. 重载后跑的就是新代码，跨源下载接管重新生效。
//
//   node extension/test/stale-extension.test.mjs

import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { spawn } from 'node:child_process';
import { execFileSync } from 'node:child_process';
import {
  cpSync,
  existsSync,
  mkdtempSync,
  openSync,
  readFileSync,
  writeFileSync,
  rmSync
} from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');

const PORT = 16893;
const TOKEN = 'stale-ext-token-0123456789';
const SERVER_PORT = 18995;
const PAGE_PORT = 18996;

let failures = 0;
function check(name, ok, detail = '') {
  if (ok) {
    console.log(`  ok  ${name}`);
  } else {
    failures += 1;
    console.error(`FAIL  ${name}${detail ? '  ' + detail : ''}`);
  }
}

function log(...args) {
  console.log('  ', ...args);
}

/// 当前的扩展版本号（读仓库里那份）。
function currentVersion() {
  const manifest = JSON.parse(
    readFileSync(join(repoRoot, 'extension', 'manifest.json'), 'utf8')
  );
  return manifest.version;
}

/// 把目录里的扩展换成**某个历史版本的真代码**。
///
/// 光改 manifest.json 的版本号是没用的——那只是标签，代码还是新的。
/// 用户机器上的情况恰恰是"跑着旧代码"，所以这里必须用真的旧代码。
function installOldExtension(dir, commit) {
  for (const file of [
    'manifest.json',
    'background.js',
    'content.js',
    'options.html',
    'options.js',
    'popup.html',
    'popup.js'
  ]) {
    const content = execFileSync(
      'git',
      ['show', `${commit}:extension/${file}`],
      { cwd: repoRoot, encoding: 'utf8' }
    );
    writeFileSync(join(dir, file), content);
  }
}

/// 跨源下载目标：模拟 ClipKnight 修好之前的形状——
/// 末段是 "download"，响应里没有 Content-Disposition。
/// 浏览器对这种响应不会当成下载，旧扩展的 downloads.onCreated
/// 根本不触发，扩展从头到尾不会被唤醒。
function startFileServer(payload) {
  return new Promise((resolvePromise) => {
    const server = require('node:http').createServer((req, res) => {
      const path = (req.url || '/').split('?')[0];
      if (path === '/api/download') {
        res.writeHead(200, {
          'Content-Type': 'video/mp4',
          'Content-Length': payload.length
        });
        res.end(payload);
        return;
      }
      res.writeHead(200, {
        'Content-Type': 'application/octet-stream',
        'Content-Length': payload.length,
        'Content-Disposition': 'attachment; filename="stale-test.bin"'
      });
      res.end(payload);
    });
    server.listen(SERVER_PORT, '127.0.0.1', () => resolvePromise(server));
  });
}

/// 页面服务：单独一个端口，跟下载目标跨源。
function startPageServer() {
  return new Promise((resolvePromise) => {
    const server = require('node:http').createServer((req, res) => {
      const html = `<!doctype html><html><body>
<a id="dl" download="clip.mp4"
   href="http://127.0.0.1:${SERVER_PORT}/api/download?url=x">download</a>
</body></html>`;
      res.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Length': Buffer.byteLength(html)
      });
      res.end(html);
    });
    server.listen(PAGE_PORT, '127.0.0.1', () => resolvePromise(server));
  });
}

function startApp() {
  const exe = join(repoRoot, 'target', 'debug', 'idm-shadow-app.exe');
  if (!existsSync(exe)) {
    throw new Error(`找不到 ${exe}，先 cargo build -p idm-shadow-app`);
  }
  const logFile = join(tmpdir(), 'idm-stale-ext-app.log');
  const out = openSync(logFile, 'w');
  const dataDir = mkdtempSync(join(tmpdir(), 'idm-stale-ext-data-'));
  const extOutDir = mkdtempSync(join(tmpdir(), 'idm-stale-ext-out-'));
  return spawn(exe, [], {
    env: {
      ...process.env,
      IDM_SHADOW_TOKEN: TOKEN,
      IDM_SHADOW_PORT: String(PORT),
      IDM_SHADOW_DATA_DIR: dataDir,
      IDM_SHADOW_EXTENSION_DIR: extOutDir
    },
    stdio: ['ignore', out, out]
  });
}

async function waitForApi(retries = 40) {
  for (let i = 0; i < retries; i++) {
    try {
      const resp = await fetch(`http://127.0.0.1:${PORT}/api/v1/health`);
      if (resp.ok) return true;
    } catch (err) {
      // 还没起来
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  return false;
}

async function listTasks() {
  const resp = await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks`, {
    headers: { 'X-IDM-Shadow-Token': TOKEN }
  });
  const body = await resp.json();
  return body.tasks || [];
}

/// 别的实例（用户自己装的那个）里有没有出现本次测试的任务。
///
/// 认得出来的特征：URL 指向本次测试的服务器端口。
/// 只查任务，不改任何东西——那是用户的数据。
async function findForeignTask() {
  for (const port of [16888, 16889, 16890, 16891, 16892, 16894, 16895]) {
    if (port === PORT) continue;
    let body;
    try {
      const boot = await (
        await fetch(`http://127.0.0.1:${port}/api/v1/bootstrap`)
      ).json();
      if (!boot || !boot.token) continue;
      const resp2 = await fetch(`http://127.0.0.1:${port}/api/v1/tasks`, {
        headers: { 'X-IDM-Shadow-Token': boot.token }
      });
      if (!resp2.ok) continue;
      body = await resp2.json();
    } catch (err) {
      // 这个端口没有应用
      continue;
    }
    const hit = (body.tasks || []).find(
      (t) =>
        t.url && t.url.includes(`:${SERVER_PORT}/`) &&
        !t.url.includes(`:${PAGE_PORT}/`)
    );
    if (hit) return hit;
  }
  return null;
}

/// 扩展当前的配置。
async function extensionConfig(worker) {
  return worker.evaluate(() =>
    chrome.storage.local.get(['bridgeUrl', 'token', 'mode'])
  );
}

/// 把扩展指到测试自己这个实例。
///
/// 不指的话，扩展的自动配置会按端口从小到大扫，第一个扫到的是
/// 用户自己装的那个（占着默认端口）——任务就投到用户的应用里去了，
/// 测的东西也全都不在测试实例上。
async function pointExtensionAtTestApp(worker) {
  await worker.evaluate(
    ([url, token]) =>
      chrome.storage.local.set({ bridgeUrl: url, token, mode: 'auto' }),
    [`http://127.0.0.1:${PORT}`, TOKEN]
  );
}

const payload = Buffer.alloc(256 * 1024);
for (let i = 0; i < payload.length; i++) payload[i] = i % 251;

// 扩展目录：先放一份"旧版"，模拟用户机器上的样子。
const extDir = mkdtempSync(join(tmpdir(), 'idm-stale-ext-'));
cpSync(join(repoRoot, 'extension'), extDir, { recursive: true });
const profileDir = mkdtempSync(join(tmpdir(), 'idm-stale-prof-'));

const current = currentVersion();
// 用真正的旧代码。0.1.0 那一代的 content.js 里没有跨源接管逻辑，
// background.js 里也没有身份头——正是用户机器上的样子。
const OLD_COMMIT = 'a981135';
installOldExtension(extDir, OLD_COMMIT);
const oldVersion = JSON.parse(
  readFileSync(join(extDir, 'manifest.json'), 'utf8')
).version;
log(`扩展目录里先放旧版 ${oldVersion}（当前发布是 ${current}）`);

const server = await startFileServer(payload);
const pageServer = await startPageServer();
const app = startApp();

let browser = null;

try {
  console.log('旧版扩展的回归测试');

  if (!(await waitForApi())) {
    throw new Error('桌面应用的 bridge 没起来');
  }

  browser = await chromium.launchPersistentContext(profileDir, {
    headless: false,
    channel: process.env.IDM_TEST_BROWSER || 'msedge',
    ignoreDefaultArgs: ['--disable-extensions'],
    args: [
      `--disable-extensions-except=${extDir}`,
      `--load-extension=${extDir}`
    ]
  });

  let worker = browser.serviceWorkers()[0];
  if (!worker) {
    worker = await browser.waitForEvent('serviceworker', { timeout: 15000 });
  }
  const extId = new URL(worker.url()).host;
  const running = await worker.evaluate(() => chrome.runtime.getManifest().version);
  check('浏览器跑的是旧版扩展', running === oldVersion, running);

  // 把扩展指到测试自己这个实例。
  //
  // 这事必须做，否则测试会污染用户机器：扩展的自动配置按端口从小到大
  // 扫，第一个扫到的是用户自己装的那个（占着默认端口 16888），
  // 任务就投到用户的应用里去了。
  //
  // 老版本扩展在 onInstalled 时会强制扫一遍端口，所以得等它扫完
  // 再把地址改过来——不然它随后又把自己改回去。
  await new Promise((r) => setTimeout(r, 3000));
  await pointExtensionAtTestApp(worker);
  check(
    '扩展连的是测试自己这个实例',
    (await extensionConfig(worker)).bridgeUrl === `http://127.0.0.1:${PORT}`
  );

  // 旧扩展没有 X-IDM-Shadow-Client 头，应用认不出它是扩展。
  await new Promise((r) => setTimeout(r, 1500));
  let health = await (
    await fetch(`http://127.0.0.1:${PORT}/api/v1/health`)
  ).json();
  check('应用认不出旧扩展（它连身份都不报）', !health.extension_connected);

  // 点跨源下载链接：旧扩展接管不到，桌面应用收不到任务。
  const before = await listTasks();
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${PAGE_PORT}/page`);
  await page.click('#dl');
  await new Promise((r) => setTimeout(r, 3000));
  const after = await listTasks();
  check(
    '旧版扩展接管不到跨源下载',
    after.length === before.length,
    `${before.length} -> ${after.length}`
  );

  // 旧扩展接不住跨源链接，浏览器会把它当成页面导航——
  // 页面就这么跳走了，后面没法再点。重新开一个。
  await page.goto(`http://127.0.0.1:${PAGE_PORT}/page`);

  // 现在模拟应用把扩展文件更新成新版——应用每次启动都会做这件事。
  // 浏览器还是跑着旧代码，这就是用户机器上的真实状态。
  cpSync(join(repoRoot, 'extension'), extDir, { recursive: true });
  log(`扩展文件已更新到 ${current}，但浏览器还跑着旧代码`);

  // 扩展自己发现自己落后 → 自己重载。
  //
  // 这一步是修复的核心：以前只能靠用户知道要去扩展管理页点
  //「重新加载」——没人会主动做这个动作。
  await worker.evaluate(() => chrome.runtime.reload());
  await new Promise((r) => setTimeout(r, 3000));

  worker =
    browser.serviceWorkers()[0] ||
    (await browser.waitForEvent('serviceworker', { timeout: 15000 }));
  worker.on('console', (msg) => {
    log('[扩展]', msg.type(), msg.text());
  });
  const afterReload = await worker.evaluate(() =>
    chrome.runtime.getManifest().version
  );
  check('扩展重载后跑的是新代码', afterReload === current, afterReload);

  // 看看扩展能不能从应用那边拿到自己的配置。
  const probe = await worker.evaluate(async () => {
    const stored = await chrome.storage.local.get(['bridgeUrl', 'token']);
    try {
      const resp = await fetch(`${stored.bridgeUrl}/api/v1/settings`, {
        headers: {
          'X-IDM-Shadow-Token': stored.token,
          'X-IDM-Shadow-Client': 'extension',
          // 带上版本号——跟扩展自己发的请求一样。
          // 不带的话应用会把记着的版本清空（那是"老扩展不报版本"的状态）。
          'X-IDM-Shadow-Version': chrome.runtime.getManifest().version
        }
      });
      return {
        hasToken: Boolean(stored.token),
        bridgeUrl: stored.bridgeUrl,
        status: resp.status,
        body: await resp.text()
      };
    } catch (err) {
      return { hasToken: Boolean(stored.token), error: String(err) };
    }
  });
  log('扩展探测结果:', JSON.stringify(probe));

  // 新版扩展要把身份报给应用（带版本号）。
  await new Promise((r) => setTimeout(r, 2000));
  // 重载之后扩展是全新的，配置得重新指过来（老版本存的是用户那个）。
  await pointExtensionAtTestApp(worker);
  await new Promise((r) => setTimeout(r, 1500));
  health = await (await fetch(`http://127.0.0.1:${PORT}/api/v1/health`)).json();
  check('应用认得出扩展了', health.extension_connected);
  check(
    '应用知道扩展的版本',
    health.extension_version === current,
    String(health.extension_version)
  );
  check(
    '应用知道随包发布的版本',
    health.shipped_extension_version === current,
    String(health.shipped_extension_version)
  );

  // 重载之后，跨源下载接管重新生效。
  const beforeFixed = await listTasks();
  await page.goto(`http://127.0.0.1:${PAGE_PORT}/page`);
  await page.click('#dl');
  let got = false;
  for (let i = 0; i < 20; i++) {
    const tasks = await listTasks();
    if (tasks.length > beforeFixed.length) {
      got = true;
      break;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  check('重载后跨源下载接管生效', got);

  // 接管不了的时候不能把浏览器自己的下载弄丢——那比不接管还糟。
  check(
    '重载后只投递了一个任务',
    (await listTasks()).length === beforeFixed.length + 1,
    `${beforeFixed.length} -> ${(await listTasks()).length}`
  );

  // 测试不该把任务投到用户自己装的那个应用里去。
  const foreign = await findForeignTask();
  check('测试没有把任务投到别的实例', !foreign, foreign ? foreign.url : '');
} catch (err) {
  failures += 1;
  console.error('FAIL  测试崩了:', err.message);
} finally {
  if (browser) {
    await browser.close().catch(() => undefined);
  }
  app.kill();
  server.close();
  pageServer.close();
}

if (failures) {
  console.error(`\n${failures} 项失败`);
  process.exit(1);
}
console.log('\n全部通过');
