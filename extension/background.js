// IDM Shadow 扩展：后台服务 worker
//
// 核心职责：拦截浏览器下载动作，把它转交给本地的 IDM Shadow 应用。
// 油猴脚本的 GM_download() 最终也会走浏览器下载栈，所以在这里统一接管，
// 用户不需要改动任何现有脚本。

const DEFAULTS = {
  bridgeUrl: 'http://127.0.0.1:16888',
  token: '',
  // off: 不接管；ask: 弹提示由用户确认；auto: 直接接管
  // 'follow-app': 跟随桌面应用的"设为默认下载工具"开关
  mode: 'ask',
  autoLaunch: true,
  handled: {}
};

// 桌面应用里勾了"设为默认下载工具"，扩展就自动接管。
// 单一事实来源在桌面应用，这里只是服从它。
let followAppMode = true;

// downloadId → 任务 id。文件名定下来后要回填。
const taskByDownload = {};

/// 改任务的文件名。
async function renameTask(taskId, filename) {
  if (!taskId || !filename) return;
  const s = await settings();
  try {
    await fetch(apiUrl(`/api/v1/tasks/${taskId}`, s), {
      method: 'PATCH',
      headers: {
        'Content-Type': 'application/json',
        'X-IDM-Shadow-Token': s.token
      },
      body: JSON.stringify({ filename })
    });
  } catch (err) {
    console.warn('[IDM Shadow] 回填文件名失败', err);
  }
}

/// 等 Chrome 把最终文件名定下来。
/// 下载项刚创建时只有临时名，要等它真正开始写文件才有真名字。
async function waitForFilename(downloadId, retries = 20) {
  for (let i = 0; i < retries; i++) {
    await new Promise((r) => setTimeout(r, 150));
    try {
      const items = await chrome.downloads.search({ id: downloadId });
      if (items.length === 0) continue;
      const name = items[0].filename || '';
      // "download" 和 "download (2)" 都是临时名
      if (name && !/^download( \(\d+\))?$/.test(name)) {
        return name.split(/[\\/]/).pop();
      }
    } catch (err) {
      // 下载可能已经被我们取消了
    }
  }
  return null;
}

/// 自己问服务器要文件名。
///
/// Chrome 给的名字在接管场景下不可靠——我们很快就把浏览器下载取消了，
/// 它可能永远停在 "download (2)" 这种临时名上。
/// 直接读响应头里的 Content-Disposition 最靠谱，跟 Chrome 的状态无关。
async function filenameFromServer(url) {
  try {
    const resp = await fetch(url, { method: 'HEAD' });
    const disposition = resp.headers.get('content-disposition');
    if (!disposition) return null;

    // filename*=UTF-8''xxx 优先
    const starMatch = /filename\*\s*=\s*([^']*)'([^']*)'([^;]+)/i.exec(disposition);
    if (starMatch) {
      try {
        return decodeURIComponent(starMatch[3].trim().replace(/^"|"$/g, ''));
      } catch (err) {
        // 解码失败就用普通的
      }
    }

    const plain = /filename\s*=\s*"?([^";]+)"?/i.exec(disposition);
    if (plain) {
      return plain[1].trim();
    }
  } catch (err) {
    // 服务器不支持 HEAD 或者网络不通
  }
  return null;
}

async function settings() {
  const stored = await chrome.storage.local.get(DEFAULTS);
  return { ...DEFAULTS, ...stored };
}

// 端口可能会被占着，应用会往后找。所以从默认端口往后试几个。
const CANDIDATE_PORTS = [16888, 16889, 16890, 16891, 16892, 16893, 16894, 16895];

/**
 * 找应用：问它要 bridge 地址和 token。
 *
 * 没有这一步的话，用户得手动把 48 位 token 从应用设置页
 * 抄到扩展设置页里——最容易卡住的一步。
 *
 * 引导接口只让浏览器扩展读（普通网站读不到），所以这样
 * 不会削弱 token 挡住"网站乱塞下载任务"的作用。
 */
async function discoverApp() {
  for (const port of CANDIDATE_PORTS) {
    const base = `http://127.0.0.1:${port}`;
    try {
      const resp = await fetch(`${base}/api/v1/bootstrap`, { method: 'GET' });
      if (!resp.ok) continue;
      const body = await resp.json();
      if (!body || !body.token) continue;
      return { bridgeUrl: base, token: body.token };
    } catch (err) {
      // 这个端口没有应用，试试下一个
    }
  }
  return null;
}

// 自动配置：找到应用就把地址和 token 填上。
async function autoConfigure(force = false) {
  const s = await settings();
  if (!force && s.token && s.bridgeUrl) return s;
  const found = await discoverApp();
  if (!found) return s;
  await chrome.storage.local.set({
    bridgeUrl: found.bridgeUrl,
    token: found.token
  });
  return { ...s, ...found };
}

/// 当前应当用什么接管方式。
function effectiveMode(s) {
  if (s.mode === 'follow-app') {
    // 桌面应用里没勾"设为默认下载工具"，就别抢浏览器的下载。
    return followAppMode ? 'auto' : 'off';
  }
  // 老版本存过 'ask'——那会儿会先弹浏览器的通知气泡，
  // 等用户点"交给 IDM Shadow"才接管。用户不想看那个气泡了，
  // 现在直接接管，弹出 IDM Shadow 自己的下载对话框。
  if (s.mode === 'ask') return 'auto';
  return s.mode;
}

/// 向桌面应用打听一下"我是不是默认下载工具"。
async function syncWithApp() {
  const s = await settings();
  if (!s.token) return;
  try {
    const resp = await fetch(apiUrl('/api/v1/settings', s), {
      headers: { 'X-IDM-Shadow-Token': s.token }
    });
    if (!resp.ok) return;
    const body = await resp.json();
    followAppMode = Boolean(body.default_download_tool);
  } catch (err) {
    // 应用没开就保持原样
  }
}

async function saveSettings(patch) {
  await chrome.storage.local.set(patch);
}

function apiUrl(path, s) {
  return `${s.bridgeUrl}${path}`;
}

// 应用是否在跑。
async function isAlive() {
  const s = await settings();
  try {
    const resp = await fetch(apiUrl('/api/v1/health', s), { method: 'GET' });
    return resp.ok;
  } catch (err) {
    return false;
  }
}

// 把 Cookie 拼成请求头用的字符串。
async function cookieHeader(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (!cookies || cookies.length === 0) return '';
    return cookies.map((c) => `${c.name}=${c.value}`).join('; ');
  } catch (err) {
    return '';
  }
}

// 投递一个下载任务给应用。
async function createTask(payload) {
  const s = await settings();
  const resp = await fetch(apiUrl('/api/v1/tasks', s), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'X-IDM-Shadow-Token': s.token
    },
    body: JSON.stringify(payload)
  });
  if (!resp.ok) {
    let message = resp.statusText;
    try {
      const body = await resp.json();
      if (body && body.error && body.error.message) message = body.error.message;
    } catch (err) {
      // 忽略，保留状态码
    }
    throw new Error(message);
  }
  return await resp.json();
}

// 唤起桌面应用（通过自定义协议）。
async function launchApp(url, name) {
  const params = new URLSearchParams();
  if (url) params.set('url', url);
  if (name) params.set('name', name);
  const target = params.toString() ? `idmshadow://dl?${params.toString()}` : 'idmshadow://';
  try {
    // 协议链接只能靠开标签页来触发，但 Edge 会把这个标签页
    // 显示成「无法访问此页面」的错误页——用完就关掉。
    const tab = await chrome.tabs.create({ url: target, active: false });
    if (tab && tab.id !== undefined) {
      setTimeout(async () => {
        try {
          await chrome.tabs.remove(tab.id);
        } catch (err) {
          // 标签页已经关了
        }
      }, 1200);
    }
  } catch (err) {
    console.warn('[IDM Shadow] 唤起应用失败', err);
  }
}

// 把一个下载项交给应用。
async function capture(item) {
  const url = item.url || '';
  if (!url) return false;
  // blob / data URL 由页面内生命周期决定，引擎取不到
  if (/^(blob|data|chrome-extension|edge):/i.test(url)) return false;

  // 还没配置过就先自动找一下应用。
  // 装完扩展就能直接用，不用手动抄 token。
  await autoConfigure();

  // 文件名优先问服务器要——Chrome 给的名字在接管场景下不可靠，
// 我们把浏览器下载取消掉之后它可能永远停在 "download (2)"。
  const serverName = await filenameFromServer(url);
  const chromeName = filenameByDownload[item.id] || item.filename;
  const finalName = serverName || chromeName;

  const payload = {
    url,
    filename: finalName || undefined,
    // chrome.downloads 用的是 referrer（两个 r），内容脚本发过来的是 referer。
    // 两个都认，免得丢掉来源页信息——不少站点靠 Referer 校验才肯给文件。
    referer: item.referrer || item.referer || undefined,
    userAgent: navigator.userAgent,
    cookies: await cookieHeader(url),
    engine: 'auto',
    autostart: true,
    // 让应用弹保存位置确认框（IDM 式流程）
    needs_confirm: true
  };

  try {
    const task = await createTask(payload);
    if (task && task.id && item.id !== undefined) {
      taskByDownload[item.id] = task.id;
    }
    // 名字还是临时名（比如 "download"），再等等 Chrome 定好
    if (task && task.id && finalName && /^download( \(\d+\))?$/.test(finalName)) {
      const final = await waitForFilename(item.id);
      if (final) await renameTask(task.id, final);
    }
    await markHandled(item.id, true);
    await updateBadge();
    return true;
  } catch (err) {
    const s = await settings();
    if (s.autoLaunch) {
      await launchApp(url, item.filename);
      // 应用起来之后重试一次
      setTimeout(async () => {
        try {
          await createTask(payload);
          await markHandled(item.id, true);
          await updateBadge();
        } catch (err2) {
          console.warn('[IDM Shadow] 重试投递失败', err2);
        }
      }, 2500);
      return true;
    }
    console.warn('[IDM Shadow] 投递失败', err);
    return false;
  }
}

// Chrome 确定最终文件名时会触发这个事件。
// 下载项刚创建时只给个临时名（"download"），真正的名字要等这里。
//
// 事件顺序：onDeterminingFilename 先于 onCreated，而 onCreated 里
// 要 await 一次网络请求，所以那时候文件名事件已经过去了。
// 因此这里先把文件名按 downloadId 记下来，onCreated 里再取。
const filenameByDownload = {};

chrome.downloads.onDeterminingFilename.addListener(async (item, suggest) => {
  const filename = suggest && suggest.filename ? suggest.filename : item.filename;
  if (filename) filenameByDownload[item.id] = filename;
});

async function markHandled(id, handled) {
  const s = await settings();
  const map = { ...s.handled };
  if (handled) {
    map[id] = Date.now();
  } else {
    delete map[id];
  }
  // 只保留最近一小时的记录
  const cutoff = Date.now() - 3600 * 1000;
  const trimmed = {};
  for (const [key, value] of Object.entries(map)) {
    if (value >= cutoff) trimmed[key] = value;
  }
  await saveSettings({ handled: trimmed });
}

async function isHandled(id) {
  const s = await settings();
  return Boolean(s.handled && s.handled[id]);
}

// 拦截浏览器下载。
chrome.downloads.onCreated.addListener(async (item) => {
  const s = await settings();
  const mode = effectiveMode(s);
  if (mode === 'off') return;
  if (await isHandled(item.id)) return;

  if (mode === 'auto') {
    const ok = await capture(item);
    if (ok) {
      try {
        await chrome.downloads.cancel(item.id);
      } catch (err) {
        // 下载可能已经结束
      }
      return;
    }
    // 接管失败（blob: 地址、应用没起来等），
    // 至少通知用户一声，别让下载凭空消失。
    await notifyAsk(item);
    return;
  }

  await notifyAsk(item);
});

async function notifyAsk(item) {
  const s = await settings();
  if (!s.token) return;

  const id = `ask-${item.id}`;
  try {
    await chrome.notifications.create(id, {
      type: 'basic',
      // MV3 的通知必须有图标，否则报
      // "Some of the required properties are missing: type, iconUrl, title and message"
      iconUrl: chrome.runtime.getURL('icon32.png'),
      title: '用 IDM Shadow 下载？',
      message: item.filename || item.url,
      buttons: [{ title: '交给 IDM Shadow' }],
      priority: 0
    });
  } catch (err) {
    console.warn('[IDM Shadow] 通知失败', err);
  }
}

chrome.notifications.onButtonClicked.addListener(async (notificationId) => {
  if (!notificationId.startsWith('ask-')) return;
  const downloadId = Number(notificationId.slice(4));
  try {
    const items = await chrome.downloads.search({ id: downloadId });
    if (items.length > 0) {
      await capture(items[0]);
      try {
        await chrome.downloads.cancel(downloadId);
      } catch (err) {
        // 忽略
      }
    }
  } catch (err) {
    console.warn('[IDM Shadow] 接管失败', err);
  }
  chrome.notifications.clear(notificationId);
});

// 右键菜单：把链接 / 视频交给应用。
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: 'idm-shadow-download',
      title: '用 IDM Shadow 下载',
      contexts: ['link', 'video', 'audio', 'image']
    });
  });
  chrome.alarms.create('refresh-badge', { periodInMinutes: 1 });
  syncWithApp();
});

// 装好扩展就自动找一次应用，把地址和 token 填上。
// 用户装完就能直接用，不用手动抄 token。
chrome.runtime.onInstalled.addListener(() => {
  autoConfigure(true);
});

// 每次浏览器启动也找一次：
// 应用可能换了端口（上一个占着），token 也可能重新生成过。
chrome.runtime.onStartup.addListener(() => {
  autoConfigure(true);
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId !== 'idm-shadow-download') return;
  const url = info.srcUrl || info.linkUrl || (tab && tab.url);
  if (!url) return;
  await capture({ url, filename: '', referer: info.pageUrl });
});

// 角标显示进行中的任务数。
async function updateBadge() {
  const s = await settings();
  if (!s.token) {
    await chrome.action.setBadgeText({ text: '' });
    return;
  }
  try {
    const resp = await fetch(apiUrl('/api/v1/tasks', s), {
      headers: { 'X-IDM-Shadow-Token': s.token }
    });
    if (!resp.ok) throw new Error('bad status');
    const body = await resp.json();
    const active = (body.tasks || []).filter((t) =>
      ['queued', 'probing', 'downloading', 'retrying'].includes(t.status)
    ).length;
    await chrome.action.setBadgeText({ text: active > 0 ? String(active) : '' });
    await chrome.action.setBadgeBackgroundColor({ color: '#2f7de1' });
  } catch (err) {
    await chrome.action.setBadgeText({ text: '!' });
    await chrome.action.setBadgeBackgroundColor({ color: '#c0392b' });
  }
}

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'refresh-badge') {
    syncWithApp();
    updateBadge();
  }
});

// 定期问桌面应用"默认下载工具"的开关有没有变。
chrome.alarms.create('sync-app-mode', { periodInMinutes: 1 });

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message && message.type === 'capture-url') {
    capture({ url: message.url, filename: message.filename || '', referer: message.referer })
      .then((ok) => sendResponse({ ok }));
    return true;
  }
  if (message && message.type === 'status') {
    isAlive()
      .then((alive) => sendResponse({ alive }))
      .catch(() => sendResponse({ alive: false }));
    return true;
  }
  if (message && message.type === 'tasks') {
    settings()
      .then((s) =>
        fetch(apiUrl('/api/v1/tasks', s), { headers: { 'X-IDM-Shadow-Token': s.token } })
      )
      .then((r) => r.json())
      .then((body) => sendResponse(body))
      .catch(() => sendResponse({ tasks: [] }));
    return true;
  }
  return false;
});

updateBadge();
