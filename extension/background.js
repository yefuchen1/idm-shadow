// IDM Shadow 扩展：后台服务 worker
//
// 核心职责：拦截浏览器下载动作，把它转交给本地的 IDM Shadow 应用。
// 油猴脚本的 GM_download() 最终也会走浏览器下载栈，所以在这里统一接管，
// 用户不需要改动任何现有脚本。

const DEFAULTS = {
  bridgeUrl: 'http://127.0.0.1:16888',
  token: '',
  // off: 不接管；ask: 弹提示由用户确认；auto: 直接接管
  mode: 'ask',
  autoLaunch: true,
  handled: {}
};

async function settings() {
  const stored = await chrome.storage.local.get(DEFAULTS);
  return { ...DEFAULTS, ...stored };
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
    await chrome.tabs.create({ url: target, active: false });
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

  const payload = {
    url,
    filename: item.filename || undefined,
    // chrome.downloads 用的是 referrer（两个 r），内容脚本发过来的是 referer。
    // 两个都认，免得丢掉来源页信息——不少站点靠 Referer 校验才肯给文件。
    referer: item.referrer || item.referer || undefined,
    userAgent: navigator.userAgent,
    cookies: await cookieHeader(url),
    engine: 'auto',
    autostart: true
  };

  try {
    await createTask(payload);
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
  if (s.mode === 'off') return;
  if (await isHandled(item.id)) return;

  if (s.mode === 'auto') {
    const ok = await capture(item);
    if (ok) {
      try {
        await chrome.downloads.cancel(item.id);
      } catch (err) {
        // 下载可能已经结束
      }
    }
    return;
  }

  // ask 模式：弹通知，用户点了才接管
  await notifyAsk(item);
});

async function notifyAsk(item) {
  const s = await settings();
  if (!s.token) return;

  const id = `ask-${item.id}`;
  try {
    await chrome.notifications.create(id, {
      type: 'basic',
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
  if (alarm.name === 'refresh-badge') updateBadge();
});

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
