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
  mode: 'auto',
  autoLaunch: true,
  // 跳过图片预览。浏览模型预览站、图库、论坛的时候，
  // 页面里的图片会被浏览器当成下载，一个预览图弹一次对话框。
  skipImages: true,
  handled: {}
};

// 图片类后缀。用来把"页面预览图"挡在接管之外。
const IMAGE_EXT = /\.(jpe?g|png|gif|webp|bmp|svg|avif|ico|tiff?)$/i;

// 打到本地 bridge 的请求都带上这个头，应用靠它判断扩展还活着
// （设置页显示"已连接 / 未连接"）。不参与鉴权——鉴权始终是 token。
const CLIENT_HEADER = 'X-IDM-Shadow-Client';
const CLIENT_VALUE = 'extension';

// 扩展自己的版本号。
//
// 用 manifest 里的版本号，而不是文件 mtime 之类的旁证——
// 这个值就是浏览器实际在跑的那份代码的版本。
function extensionVersion() {
  try {
    const manifest = chrome.runtime.getManifest();
    return (manifest && manifest.version) || '';
  } catch (err) {
    return '';
  }
}

// 请求头里带上扩展版本。
//
// 关键用途：浏览器会缓存已解压扩展的注册信息——磁盘上的文件换了新版，
// 浏览器照样跑着旧代码，而且旧代码根本不发这个头。那时候应用这边
// 看到的就是"扩展没连上"，用户看到的是"点了下载一点反应都没有"。
// 带上版本号，应用就能把"扩展是旧版"和"扩展没装"区分开。
function clientHeaders(extra) {
  return Object.assign(
    {
      [CLIENT_HEADER]: CLIENT_VALUE,
      'X-IDM-Shadow-Version': extensionVersion()
    },
    extra || {}
  );
}

// 只取文件名，不要路径。
//
// Chrome 的 downloads.onCreated 给的 item.filename 是完整路径
// （C:\Users\ROG\Desktop\x.rar），服务器在 Content-Disposition 里
// 也经常塞完整路径。直接用的话文件名会变成
// C__Users_ROG_Desktop_x.rar 这种压平过的样子。
function basename(name) {
  if (!name) return null;
  const text = String(name);
  const cut = Math.max(text.lastIndexOf('/'), text.lastIndexOf('\\'));
  const base = (cut >= 0 ? text.slice(cut + 1) : text).trim();
  return base || null;
}

function looksLikeImage(text) {
  if (!text) return false;
  // 去掉查询串再判断后缀
  const path = String(text).split(/[?#]/)[0];
  return IMAGE_EXT.test(path);
}

// 本地文件：浏览器不会为这些触发"下载"，但页面另存、file:// 打开、
// 或者某些站点用 a[download] 指向本地资源时会。
//
// 用户右键另存的图片已经在他自己的盘上了，没有"下载"这回事——
// 拦下来只会让人莫名其妙。交给浏览器自己处理。
function isLocalFile(item) {
  const url = item.url || '';
  if (/^file:/i.test(url)) return true;
  // 已经落到本地的路径（Chrome 给的 filename 是完整路径）。
  // 注意只认盘符开头的绝对路径，别把普通的相对文件名误判。
  if (/^[a-zA-Z]:[\\/]/.test(item.filename || '')) return true;
  return false;
}

// 是不是页面预览图那种"不该当下载接管"的图片。
//
// 浏览模型预览站、图库、论坛的时候，页面里的图片会被浏览器当成下载，
// 一个预览图弹一次对话框，很烦。
async function shouldSkipImage(item) {
  const s = await settings();
  if (!s.skipImages) return false;
  return looksLikeImage(item.url) || looksLikeImage(item.filename);
}

// 这个下载要不要放过去不管。
//
// isLocalFile 只在浏览器那边有意义（Chrome 的 item.filename 是完整路径，
// 用来看是不是用户磁盘上已有的东西），所以只给 onCreated 那条路用。
async function shouldSkip(item) {
  if (isLocalFile(item)) return true;
  return shouldSkipImage(item);
}

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
      headers: clientHeaders({
        'Content-Type': 'application/json',
        'X-IDM-Shadow-Token': s.token
      }),
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

/// 浏览器还没定好文件名时给的临时名。
///
/// Chrome 在文件名确定之前会先用 "download"、"download (2)" 这种占位名，
/// 下载中还会加 ".crdownload" 后缀。这些不是用户要的文件名，
/// 传给引擎只会让落盘文件变成一个光秃秃的 "download"。
/// 引擎那边有同样的判断（engine/src/task.rs）。
function isTemporaryName(name) {
  if (!name) return true;
  const lower = String(name).trim().toLowerCase();
  if (!lower) return true;
  const stem = lower.endsWith('.crdownload') ? lower.slice(0, -'.crdownload'.length) : lower;
  if (stem === 'download') return true;
  return /^download \(\d+\)$/.test(stem);
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
  // 已经配好的地址先试。
  //
  // 这一步很要紧：机器上可能同时跑着好几个 IDM Shadow 实例
  // （开发时、测试时、自动化脚本都会各起一个）。以前只按端口从小到大扫，
  // 扫到的第一个是谁全看运气——用户明明装的是另一个实例，扩展却连上了
  // 别人家那个，任务就投到不该投的地方去了。
  //
  // 用户手动配过的地址优先，这个才合乎直觉。
  const configured = await configuredBridge();
  if (configured) return configured;

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

/// 已经配好的那个应用还活着吗？活着就还用它。
async function configuredBridge() {
  const s = await settings();
  if (!s.bridgeUrl || !s.token) return null;
  try {
    const resp = await fetch(`${s.bridgeUrl}/api/v1/bootstrap`, { method: 'GET' });
    if (!resp.ok) return null;
    const body = await resp.json();
    if (!body || !body.token) return null;
    // token 对不上说明那边换了新实例，就当没配过。
    if (body.token !== s.token) return null;
    return { bridgeUrl: s.bridgeUrl, token: s.token };
  } catch (err) {
    // 连不上就是没配好，退回扫端口。
    return null;
  }
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
      headers: clientHeaders({ 'X-IDM-Shadow-Token': s.token })
    });
    if (!resp.ok) return;
    const body = await resp.json();
    followAppMode = Boolean(body.default_download_tool);
    await reloadIfOutdated(body.shipped_extension_version);
  } catch (err) {
    // 应用没开就保持原样
  }
}

/// 桌面应用带的扩展比我现在这版新，就把自己重载一下。
///
/// 为什么需要这个：浏览器把已解压扩展的注册信息缓存住了。应用每次启动
/// 都会把新版的扩展文件写进用户实际加载的那个目录，但浏览器照样跑着
/// 旧代码——于是跨源下载接管那套逻辑压根不存在，用户点下载一点反应都没有。
///
/// 以前这件事只能靠用户自己知道要去扩展管理页点「重新加载」。那是个
/// 没人会主动去做的动作，症状又是"点了下载没反应"这种说不清的现象，
/// 于是用户只会觉得软件坏了。
///
/// 这里让扩展自己发现版本落后，然后自己重载：文件已经是新的了，
/// 重载一下就等于点了一次「重新加载」。
async function reloadIfOutdated(shippedVersion) {
  if (!shippedVersion) return;
  const mine = extensionVersion();
  if (!mine) return;
  // 只有"我比应用旧"才重载。
  //
  // 版本号相等不用管；我比应用新（比如用户先手动装了个更新的扩展）
  // 也不能管——那会让扩展反复把自己降级回去。
  if (compareVersions(mine, shippedVersion) >= 0) return;

  // 十分钟内只试一次。
  //
  // 这个时间戳必须存在 storage 里，不能放模块变量——重载会把整个
  // service worker 重来一遍，模块变量也跟着清零。放内存里的话，
  // "重载了也没生效"就会变成不停重启扩展的死循环，比不更新还糟糕。
  const now = Date.now();
  const stored = await chrome.storage.local.get({ lastReloadAttempt: 0 });
  const last = stored.lastReloadAttempt || 0;
  if (now - last < 10 * 60 * 1000) return;
  await chrome.storage.local.set({ lastReloadAttempt: now });

  console.log(
    `[IDM Shadow] 扩展是旧版（${mine} -> ${shippedVersion}），重新加载`
  );
  try {
    chrome.runtime.reload();
  } catch (err) {
    console.warn('[IDM Shadow] 自动重新加载失败', err);
  }
}

/// 比两个点分版本号。
///
/// 返回负数表示 a 比 b 老，0 表示一样，正数表示 a 更新。
/// 不是数字的部分按字符串比——版本号格式万一变了，也比"一律当成相等"
/// 要好：最坏的结果是不重载，而不会陷入重载循环。
function compareVersions(a, b) {
  const pa = String(a || '').split('.');
  const pb = String(b || '').split('.');
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i++) {
    const na = parseInt(pa[i], 10);
    const nb = parseInt(pb[i], 10);
    const va = Number.isNaN(na) ? (pa[i] || '') : na;
    const vb = Number.isNaN(nb) ? (pb[i] || '') : nb;
    if (va < vb) return -1;
    if (va > vb) return 1;
  }
  return 0;
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
    headers: clientHeaders({
      'Content-Type': 'application/json',
      'X-IDM-Shadow-Token': s.token
    }),
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

/// 投递一次，失败返回 null 而不是抛异常。
async function tryCreateTask(payload) {
  try {
    return await createTask(payload);
  } catch (err) {
    return null;
  }
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
  // 页面预览图之类的不接管。
  //
  // 这个检查放在 capture 里而不是只在 onCreated 里，
  // 是因为内容脚本那条入口（跨源 a[download]）也要守同样的规矩。
  // 放在 autoConfigure 之前，免得为了看一眼图片就先去连一遍应用。
  if (await shouldSkipImage(item)) return false;

  // 还没配置过就先自动找一下应用。
  // 装完扩展就能直接用，不用手动抄 token。
  await autoConfigure();

  const chromeName = filenameByDownload[item.id] || item.filename;
  // 只取文件名部分。Chrome 给的可能是带路径的完整路径。
  const rawName = basename(chromeName);
  // 临时名（"download"、"download (2)"）不是真名字，别传给引擎——
  // 引擎会自己问服务器要名字，并按 Content-Type 补扩展名。
  const finalName = rawName && !isTemporaryName(rawName) ? rawName : null;

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

  let task = await tryCreateTask(payload);

  // token 过期、应用换了端口、或者应用刚更新过：
  // 重新从引导接口拿一次地址和 token 再试。
  // 以前只在 token 为空时才自动配置，所以拿着旧 token 会一直 401，
  // 而且每次失败都会把浏览器下载取消掉。
  if (!task) {
    const found = await discoverApp();
    if (found) {
      await saveSettings({ bridgeUrl: found.bridgeUrl, token: found.token });
      task = await tryCreateTask(payload);
    }
  }

  // 应用没起来：唤起它，然后**等它真的接住任务**再返回。
  //
  // 以前这里是 fire-and-forget：先返回 true，调用方立刻取消浏览器下载，
  // 2.5 秒后再异步重试。应用冷启动常慢于 2.5 秒，重试失败就没人管了——
  // 用户看到的是"点了下载，浏览器不下了，IDM 也没弹框"，文件凭空消失。
  if (!task) {
    const s = await settings();
    if (s.autoLaunch) {
      await launchApp(url, item.filename);
      for (let i = 0; i < 10 && !task; i++) {
        await new Promise((r) => setTimeout(r, 800));
        task = await tryCreateTask(payload);
        if (!task) {
          const found = await discoverApp();
          if (found) {
            await saveSettings({ bridgeUrl: found.bridgeUrl, token: found.token });
            task = await tryCreateTask(payload);
          }
        }
      }
    }
  }

  // 始终没接住：返回 false，让浏览器自己把这文件下完。
  // 宁可这次没走 IDM，也不能把下载弄丢。
  if (!task) {
    console.warn('[IDM Shadow] 投递失败，保留浏览器下载');
    return false;
  }

  if (task.id && item.id !== undefined) {
    taskByDownload[item.id] = task.id;
  }
  if (item.id !== undefined) {
    await markHandled(item.id, true);
  }
  await updateBadge();
  return true;
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
    // 过滤掉的（比如页面预览图）不接管。
    // 也不取消浏览器的下载——让它自己下下去，我们不打扰用户。
    if (await shouldSkip(item)) {
      return;
    }
    const ok = await capture(item);
    if (ok) {
      try {
        await chrome.downloads.cancel(item.id);
      } catch (err) {
        // 下载可能已经结束
      }
      return;
    }
    // 接管失败（blob: 地址、应用没起来等）。
    //
    // 什么都不做——不弹气泡，也不取消浏览器下载，让浏览器自己下完。
    // 以前这里会弹一个"用 IDM Shadow 下载？"的通知，可那句话
    // 又没取消下载，用户看到的只是莫名其妙地问一句要不要用 IDM，
    // 点不点都一样。要的是"能用 IDM 就用，用不了就别吭声"。
    return;
  }

  // mode 只剩 'auto' 和 'off'（'ask' 在 effectiveMode 里已经归到 auto，
  // 'follow-app' 也只会返回 auto 或 off），走到这儿说明是没见过的值，
  // 同样什么都不做。
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
      headers: clientHeaders({ 'X-IDM-Shadow-Token': s.token })
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
    // 内容脚本发来的接管请求也要守模式。
    //
    // 用户在设置里选了"不接管"，内容脚本却照样把下载抢走的话，
    // 那个开关就形同虚设了。
    settings()
      .then((s) => {
        if (effectiveMode(s) === 'off') return false;
        return capture({
          url: message.url,
          filename: message.filename || '',
          referer: message.referer
        });
      })
      .then((ok) => sendResponse({ ok }))
      .catch(() => sendResponse({ ok: false }));
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
        fetch(apiUrl('/api/v1/tasks', s), {
          headers: clientHeaders({ 'X-IDM-Shadow-Token': s.token })
        })
      )
      .then((r) => r.json())
      .then((body) => sendResponse(body))
      .catch(() => sendResponse({ tasks: [] }));
    return true;
  }
  return false;
});

// service worker 起来就先跟应用打声招呼。
//
// 这一步不能省：自愈逻辑会调 chrome.runtime.reload()，重载之后
// service worker 是全新的，onInstalled / onStartup 都不会再触发。
// 不主动连一次的话，扩展要等下一次闹钟（最多一分钟）才会被应用
// 认出来——用户那边看着就是「还是没反应」。
//
// 顺便把应用带的扩展版本问回来：比自己的旧就自己重载。
syncWithApp();
updateBadge();
