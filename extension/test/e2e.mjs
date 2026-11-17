// 扩展的浏览器实机验证。
//
// 用 Playwright 起一个带扩展的 Chromium（persistent context 是 MV3 扩展的硬要求），
// 打开一个会触发浏览器下载的页面，然后验证：
//   1. 浏览器下载被扩展接管
//   2. 桌面应用收到了任务
//   3. 文件真的下载完了
//
//   node extension/test/e2e.mjs

import { chromium } from 'playwright';
import { spawn } from 'node:child_process';
import { cpSync, existsSync, mkdtempSync, rmSync, openSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');
const extDir_source = join(repoRoot, 'extension');
// 扩展路径里有中文时 Chromium 启不来（spawn UNKNOWN），
// 先复制到纯 ASCII 的临时目录再用。
//
// IDM_SHADOW_EXT_DIR 可以指定别的目录——用来直接验证
// 用户实际加载的那个解压目录（比如 C:\idm-shadow-ext）。
let extDir;
let asciiRoot = null;
if (process.env.IDM_SHADOW_EXT_DIR) {
  extDir = process.env.IDM_SHADOW_EXT_DIR;
  console.log(`  用指定的扩展目录: ${extDir}`);
} else {
  asciiRoot = mkdtempSync(join(tmpdir(), 'idm-ext-ascii-'));
  extDir = join(asciiRoot, 'extension');
  cpSync(extDir_source, extDir, { recursive: true });
}

// 测试用独立端口，跟用户正在跑的实例不冲突。
const PORT = 16890;
const TOKEN = 'e2e-extension-token-0123456789';
const SERVER_PORT = 18997;
// 第二个端口。用来做"跨源"——同源的话浏览器的行为完全不一样，
// 跨源 a[download] 才是那个浏览器当成导航、扩展根本不会被唤醒的场景。
const PAGE_PORT = 18998;

function log(...args) {
  console.log('  ', ...args);
}

function startFileServer(payload) {
  return new Promise((resolvePromise) => {
    const server = require('node:http').createServer((req, res) => {
      const path = (req.url || '/').split('?')[0];

      // 跨源下载目标：模拟 ClipKnight 修好之前的形状——
      // 末段是 "download"，响应里没有 Content-Disposition。
      // 浏览器对这种响应不会当成下载，扩展的 downloads.onCreated 不会触发。
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
        'Content-Disposition': 'attachment; filename="e2e-download.bin"'
      });
      res.end(payload);
    });
    server.listen(SERVER_PORT, '127.0.0.1', () => resolvePromise(server));
  });
}

// 页面服务：单独一个端口，跟下载目标跨源。
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
  const logFile = join(tmpdir(), 'idm-ext-e2e-app.log');
  const out = openSync(logFile, 'w');
  // 独立端口 + 独立数据目录：测试不该碰到用户正在跑的实例，
  // 也不该动用户真实的任务库。
  const dataDir = mkdtempSync(join(tmpdir(), 'idm-ext-e2e-data-'));
  return spawn(exe, [], {
    env: {
      ...process.env,
      IDM_SHADOW_TOKEN: TOKEN,
      IDM_SHADOW_PORT: String(PORT),
      IDM_SHADOW_DATA_DIR: dataDir
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

// 应用有单实例机制：已经在跑的话，测试再启一个会被转交给它，
// 于是测试里的 token 和实际服务的对不上，排查起来很费劲。
// 所以先看一眼端口是不是已经有人占着。
async function portAlreadyInUse(port) {
  try {
    const resp = await fetch(`http://127.0.0.1:${port}/api/v1/health`);
    return resp.ok;
  } catch (err) {
    return false;
  }
}

async function listTasks() {
  const resp = await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks`, {
    headers: { 'X-IDM-Shadow-Token': TOKEN }
  });
  const body = await resp.json();
  return body.tasks || [];
}

// 确认连到的是测试自己启的那个实例。
// 应用是单实例的，端口上要是坐着别的东西（哪怕 token 碰巧对得上），
// 后面"扩展没投进来"这种报错会让人查错方向。
async function checkOwnInstance() {
  try {
    const resp = await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks`, {
      headers: { 'X-IDM-Shadow-Token': TOKEN }
    });
    return resp.ok;
  } catch (err) {
    return false;
  }
}

/// 清掉上次测试留下的任务和文件，保证每次都是干净的环境。
async function resetState() {
  const resp = await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks`, {
    headers: { 'X-IDM-Shadow-Token': TOKEN }
  });
  const body = await resp.json();
  for (const task of body.tasks || []) {
    await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks/${task.id}?delete_file=true`, {
      method: 'DELETE',
      headers: { 'X-IDM-Shadow-Token': TOKEN }
    });
  }
}
const payload = Buffer.alloc(512 * 1024);
for (let i = 0; i < payload.length; i++) payload[i] = i % 251;

const profileDir = mkdtempSync(join(tmpdir(), 'idm-ext-e2e-'));
const server = await startFileServer(payload);
const pageServer = await startPageServer();
// 应用是单实例的：已经有一个在跑的话，测试启的那个会被转交给它，
// 于是测试里的 token 和实际服务的对不上——表现是"扩展没投进来"，
// 但根因在这儿，很容易查错方向。所以先看一眼端口有没有人占着。
if (await portAlreadyInUse(PORT)) {
  console.error('端口 ' + PORT + ' 上已经有 IDM Shadow 在跑了，请先关掉它再跑这个测试。');
  server.close();
  pageServer.close();
  rmSync(profileDir, { recursive: true, force: true });
  process.exit(1);
}
const app = startApp();

let browser = null;
let failures = 0;

function check(name, ok, detail = '') {
  if (ok) {
    console.log(`  ok  ${name}`);
  } else {
    failures += 1;
    console.error(`FAIL  ${name}${detail ? '  ' + detail : ''}`);
  }
}

try {
  console.log('扩展实机验证');

  if (!(await waitForApi())) {
    throw new Error('桌面应用的 bridge 没起来');
  }
  log('bridge 已就绪');

  // 确认连到的是测试自己启的实例（token 对得上）。
  // 不然"扩展没投进来"这种报错会让人查错方向。
  if (!(await checkOwnInstance())) {
    throw new Error('端口上的服务不是测试启的那个实例（token 对不上）');
  }

  await resetState();

  browser = await chromium.launchPersistentContext(profileDir, {
    headless: false,
    // 用系统装的 Edge/Chrome：Playwright 自带的 Chromium 在这台机器上
    // 启不起来（并行配置不正确），系统浏览器没这个问题。
    channel: process.env.IDM_TEST_BROWSER || 'msedge',
    // Playwright 默认会加 --disable-extensions，和加载扩展冲突，得去掉
    ignoreDefaultArgs: ['--disable-extensions'],
    args: [
      `--disable-extensions-except=${extDir}`,
      `--load-extension=${extDir}`
    ]
  });

  // 等扩展的 service worker 起来
  let worker = browser.serviceWorkers()[0];
  if (!worker) {
    worker = await browser.waitForEvent('serviceworker', { timeout: 15000 });
  }
  const extId = new URL(worker.url()).host;
  log('扩展已加载: ' + extId);

  // 配置扩展：地址、token、auto 模式
  const optPage = await browser.newPage();
  await optPage.goto(`chrome-extension://${extId}/options.html`);
  await optPage.fill('#bridgeUrl', `http://127.0.0.1:${PORT}`);
  await optPage.fill('#token', TOKEN);
  await optPage.selectOption('#mode', 'auto');
  await optPage.click('#save');
  log('扩展设置已写入');

  // 「自动配置」按钮：装好扩展后不用手抄 token。
  // 顺带验证引导接口在真实浏览器里能通（Origin 是扩展 origin）。
  await optPage.fill('#token', '');
  await optPage.click('#auto');
  await optPage.waitForFunction(
    () => document.getElementById('autoMsg').textContent.includes('配置好了'),
    { timeout: 10000 }
  ).catch(() => {});
  const autoMsg = await optPage.textContent('#autoMsg');
  check('自动配置能找到应用并填好 token', autoMsg.includes('配置好了'), autoMsg);

  // 切回 auto 模式继续测接管。
  //
  // 注意：上一步"自动配置"会把扩展指向**第一个**能应答的端口。
  // 机器上如果还装着 IDM Shadow，它占着 16888，扩展就被指到那个
  // 已安装的实例上去了（token 对不上），后面的接管全都测不到。
  // 所以这里明确指回测试自己的实例。
  await optPage.fill('#bridgeUrl', `http://127.0.0.1:${PORT}`);
  await optPage.fill('#token', TOKEN);
  await optPage.selectOption('#mode', 'auto');
  await optPage.click('#save');

  const before = await listTasks();

  // 打开一个会触发下载的页面
  const page = await browser.newPage();
  // 直接导航到文件会触发下载，Playwright 会抛 "Download is starting"，
  // 这是正常的——说明浏览器确实认了这个下载。
  let browserTriggeredDownload = false;
  try {
    await page.goto(`http://127.0.0.1:${SERVER_PORT}/file.bin`);
  } catch (err) {
    if (/Download is starting/i.test(err.message)) {
      browserTriggeredDownload = true;
      log('浏览器确实触发了下载');
    } else {
      throw err;
    }
  }
  check('浏览器触发了下载动作', browserTriggeredDownload);

  // 等桌面应用收到任务（只看这次新建的，别撞上历史任务）
  let task = null;
  for (let i = 0; i < 40; i++) {
    const tasks = await listTasks();
    const found = tasks.find(
      (t) => t.url.includes(`:${SERVER_PORT}/file.bin`) && !before.some((b) => b.id === t.id)
    );
    if (found) {
      task = found;
      break;
    }
    await new Promise((r) => setTimeout(r, 500));
  }

  check('扩展把浏览器下载交给了桌面应用', Boolean(task),
    task ? '' : '桌面应用没收到任务');

  if (task) {
    log(`任务: ${task.filename} 状态=${task.status}`);
    check('走的是 IDM 式确认流程', task.needs_confirm === true);

    // 文件名是 Chrome 之后才定的，扩展会回填给桌面应用
    let named = false;
    for (let i = 0; i < 20; i++) {
      const tasks = await listTasks();
      const cur = tasks.find((t) => t.id === task.id);
      if (cur && cur.filename === 'e2e-download.bin') {
        named = true;
        task = cur;
        break;
      }
      await new Promise((r) => setTimeout(r, 500));
    }
    check('任务带上了正确的文件名', named, task.filename);

    // 任务在等用户确认保存位置——这正是 IDM 式流程该有的样子。
    // 界面上会弹"下载此文件"，用户点了才转存。
    let waiting = false;
    for (let i = 0; i < 20; i++) {
      const tasks = await listTasks();
      const cur = tasks.find((t) => t.id === task.id);
      if (cur && cur.status === 'awaiting_confirm') {
        waiting = true;
        task = cur;
        break;
      }
      await new Promise((r) => setTimeout(r, 250));
    }
    check('任务在等用户确认保存位置', waiting, task.status);

    // 模拟用户在弹窗里点"直接下载"（不指定目录，用默认位置）
    await fetch(`http://127.0.0.1:${PORT}/api/v1/tasks/${task.id}/confirm`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-IDM-Shadow-Token': TOKEN
      },
      body: JSON.stringify({})
    });

    let done = false;
    for (let i = 0; i < 60; i++) {
      const tasks = await listTasks();
      const cur = tasks.find((t) => t.id === task.id);
      if (cur && cur.status === 'completed') {
        done = true;
        task = cur;
        break;
      }
      if (cur && cur.status === 'error') {
        log('任务出错:', cur.error);
        break;
      }
      await new Promise((r) => setTimeout(r, 500));
    }
    check('任务下载完成', done);
    check('文件大小正确', task.downloaded === payload.length,
      `${task.downloaded} != ${payload.length}`);
  }

  const after = await listTasks();
  check('只投递了一个任务', after.length === before.length + 1,
    `${before.length} -> ${after.length}`);

  // 跨源 a[download]：浏览器会当成页面导航的那一种。
  //
  // 目标地址的响应里没有 Content-Disposition，浏览器不会把它当成下载，
  // chrome.downloads.onCreated 根本不触发——扩展从头到尾不会被唤醒。
  // 以前这就是"点了下载，一点反应都没有"的根因。
  //
  // 现在由内容脚本在捕获阶段拦下来，交给后台投递。
  const beforeCross = await listTasks();
  const pageX = await browser.newPage();
  await pageX.goto(`http://127.0.0.1:${PAGE_PORT}/page`);
  await pageX.click('#dl');

  let crossTask = null;
  for (let i = 0; i < 40; i++) {
    const tasks = await listTasks();
    const found = tasks.find(
      (t) =>
        t.url.includes('/api/download') &&
        !beforeCross.some((b) => b.id === t.id)
    );
    if (found) {
      crossTask = found;
      break;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  check('跨源 a[download] 也会被接管', Boolean(crossTask),
    crossTask ? '' : '桌面应用没收到任务');

  if (crossTask) {
    // 文件名：内容脚本给的 download 属性是 "clip.mp4"，
    // 引擎再用 Content-Type 核对一次扩展名。
    check('跨源下载的文件名带上了扩展名',
      crossTask.filename.endsWith('.mp4'),
      crossTask.filename);

    // 下载要真的跑起来。
    let downloading = false;
    for (let i = 0; i < 30; i++) {
      const tasks = await listTasks();
      const cur = tasks.find((t) => t.id === crossTask.id);
      if (cur && (cur.status === 'awaiting_confirm' || cur.status === 'downloading')) {
        downloading = true;
        crossTask = cur;
        break;
      }
      if (cur && cur.status === 'error') {
        log('跨源任务出错:', cur.error);
        break;
      }
      await new Promise((r) => setTimeout(r, 250));
    }
    check('跨源任务真的在下载', downloading, crossTask.status);

    // 只该有一个任务——不能既接管又让浏览器下一遍。
    const afterCross = await listTasks();
    check('跨源下载只投递了一个任务',
      afterCross.length === beforeCross.length + 1,
      `${beforeCross.length} -> ${afterCross.length}`);
  }

  // off 模式：不接管，下载留给浏览器。
  //
  // 以前的版本有 'ask'（弹通知让用户确认），后来去掉了——
  // 只弹气泡又不取消下载，用户点不点结果都一样。
  const beforeOff = await listTasks();
  await optPage.selectOption('#mode', 'off');
  await optPage.click('#save');

  // 看看扩展的 service worker 有没有报错
  const sw = browser.serviceWorkers()[0];
  const errors = [];
  if (sw) {
    sw.on('console', (msg) => {
      if (msg.type() === 'error') errors.push(msg.text());
    });
  }

  const page2 = await browser.newPage();
  try {
    await page2.goto(`http://127.0.0.1:${SERVER_PORT}/file2.bin`);
  } catch (err) {
    if (!/Download is starting/i.test(err.message)) throw err;
  }
  await new Promise((r) => setTimeout(r, 2500));

  check('off 模式没有 service worker 报错', errors.length === 0,
    errors.join(' | '));

  // off 模式不该自动投递给应用
  const afterOff = await listTasks();
  check('off 模式不会自动接管', afterOff.length === beforeOff.length,
    `${beforeOff.length} -> ${afterOff.length}`);

  // popup 的「打开主界面」不该开出标签页。
  //
  // 以前这里用 chrome.tabs.create({ url: 'idmshadow://' })，
  // Edge 会开一个标签页去加载协议链接，然后把
  // 「无法访问此页面 / localhost 拒绝连接」的错误页留在那儿。
  const popupPage = await browser.newPage();
  await popupPage.goto(`chrome-extension://${extId}/popup.html`);
  // 等前一步的下载标签页稳定下来再数，避免它自己关掉造成误判。
  await new Promise((r) => setTimeout(r, 800));
  const tabsBefore = browser.pages().length;
  await popupPage.click('#open');
  await new Promise((r) => setTimeout(r, 1500));
  const tabsAfter = browser.pages().length;
  // 关键不是数量不变（别的标签页可能自己关掉），而是没有留下
  // 一个加载协议链接的错误页。
  const stray = browser
    .pages()
    .filter((p) => /^idmshadow:|^chrome-error:/.test(p.url()));
  check('点「打开主界面」不会开出错误标签页',
    tabsAfter <= tabsBefore && stray.length === 0,
    `${tabsBefore} -> ${tabsAfter}; stray=${stray.map((p) => p.url()).join(',')}`);
} catch (err) {
  failures += 1;
  console.error('FAIL  测试执行出错:', err.message);
} finally {
  if (browser) await browser.close().catch(() => {});
  app.kill();
  server.close();
  pageServer.close();
  rmSync(profileDir, { recursive: true, force: true });
  if (asciiRoot) rmSync(asciiRoot, { recursive: true, force: true });
}

console.log('');
if (failures > 0) {
  console.error(`${failures} 项失败`);
  process.exit(1);
}
console.log('全部通过');
