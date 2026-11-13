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
import { cpSync, existsSync, mkdtempSync, rmSync } from 'node:fs';
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
const asciiRoot = mkdtempSync(join(tmpdir(), 'idm-ext-ascii-'));
const extDir = join(asciiRoot, 'extension');
cpSync(extDir_source, extDir, { recursive: true });

const PORT = 16888;
const TOKEN = 'e2e-extension-token-0123456789';
const SERVER_PORT = 18997;

function log(...args) {
  console.log('  ', ...args);
}

function startFileServer(payload) {
  return new Promise((resolvePromise) => {
    const server = require('node:http').createServer((req, res) => {
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

function startApp() {
  const exe = join(repoRoot, 'target', 'debug', 'idm-shadow-app.exe');
  if (!existsSync(exe)) {
    throw new Error(`找不到 ${exe}，先 cargo build -p idm-shadow-app`);
  }
  return spawn(exe, [], {
    env: { ...process.env, IDM_SHADOW_TOKEN: TOKEN },
    stdio: 'ignore'
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
} catch (err) {
  failures += 1;
  console.error('FAIL  测试执行出错:', err.message);
} finally {
  if (browser) await browser.close().catch(() => {});
  app.kill();
  server.close();
  rmSync(profileDir, { recursive: true, force: true });
  rmSync(asciiRoot, { recursive: true, force: true });
}

console.log('');
if (failures > 0) {
  console.error(`${failures} 项失败`);
  process.exit(1);
}
console.log('全部通过');
