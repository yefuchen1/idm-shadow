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

// 没有后缀的图片接口。摄像头/NAS/图床经常把预览图放在
// `/api/capture_image`、`/snapshot`、`/thumbnail` 这类路径上，
// URL 和文件名都没有扩展名，只看后缀会漏过去。
const IMAGE_PATH =
  /(?:^|\/)(?:api\/)?(?:capture[_-]?image|snapshot|screenshot|thumbnail|thumbs?|preview|cover|poster|avatar|favicon|frame|mjpeg|stream|photo|picture|images?|img)(?:[\/._-]|$)/i;

// 查询串里明确写了图片格式的（`?format=jpg`、`?mime=image/png`）。
const IMAGE_QUERY =
  /(?:^|[?&])(?:format|ext|type|mime|content-?type)=(?:image%2f|image\/|jpe?g|png|gif|webp|bmp|avif|svg)/i;

/// 任务来源。写进任务库，界面上直接给用户看。
///
/// 这不是"锦上添花"的字段：事故里 7 个任务全是浏览器恢复的旧下载，
/// 但任务库里看不出任何区别。有了来源，误接管一眼就能认出来。
const SOURCE = {
  USER: 'user',
  PAGE: 'page',
  RESTORED: 'restored',
  EXTENSION: 'extension'
};

/// 浏览器恢复的旧下载，`startTime` 会保留原始时间。
/// 超过这个年龄的 `onCreated` 不可能是刚刚的用户点击。
const RESTORED_AGE_MS = 10 * 60 * 1000;

/// 用户点击/右键之后，多久内发生的下载算"用户发起的"。
///
/// 页面里的下载经常是异步的：点击 → 请求签名地址 → 再触发下载。
/// 30 秒足够覆盖这条链路，又不会让几分钟后的页面自动下载蹭到。
const GESTURE_TTL_MS = 30 * 1000;
const CONTEXT_GESTURE_TTL_MS = 2 * 60 * 1000;

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
  const raw = String(text).trim();
  let path = raw;
  let search = '';

  // 有协议头的按 URL 解析，只看路径和查询串。
  //
  // 不能直接对整串做路径匹配：`https://images.example.com/x.zip`
  // 会把主机名里的 "images" 也匹配进来，把压缩包当成图片。
  if (/^[a-z][a-z0-9+.-]*:/i.test(raw)) {
    try {
      const url = new URL(raw);
      path = url.pathname;
      search = url.search;
    } catch (err) {
      return false;
    }
  } else {
    const cut = raw.search(/[?#]/);
    if (cut >= 0) {
      path = raw.slice(0, cut);
      search = raw.slice(cut);
    }
  }

  return (
    IMAGE_EXT.test(path) || IMAGE_PATH.test(path) || IMAGE_QUERY.test(search)
  );
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
  if (looksLikeImage(item.url) || looksLikeImage(item.filename)) return true;

  // 服务器已经告诉我们这是图片。`onCreated` 阶段 mime 有时还没到，
  // 但恢复的下载、带 Content-Disposition 的响应通常已经有了。
  const mime = item.mime || item.mimeType || '';
  if (/^image\//i.test(String(mime).trim())) return true;
  return false;
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

// 最近发生过的用户手势（页面里的点击 / 右键）。
//
// 浏览器不告诉扩展"这个下载是不是用户点的"。`downloads.onCreated`
// 对用户点击、页面脚本自动下载、浏览器恢复旧下载一视同仁。
// 内容脚本在真实点击时发一条消息过来，这里记一小段时间；
// 只有带着手势的下载才允许接管。
//
// 放内存里就够：点击和下载几乎同时发生，service worker 不会在这中间
// 被回收。真被回收了，最坏结果是这次交给浏览器下载，不会误接管。
const recentGestures = [];

function stripHash(url) {
  return String(url || '').split('#')[0];
}

/// 取 origin + pathname 用来比手势。
///
/// 不直接比整串：浏览器给的 referrer 和页面自己的 location.href
/// 经常差在尾斜杠、端口写法、或者重定向后的路径上。按整串比会把
/// 用户真正的点击判成"没有手势"，正常的下载就接不上了。
function gestureKey(url) {
  const raw = stripHash(url);
  if (!raw) return '';
  try {
    const u = new URL(raw);
    return `${u.origin}${u.pathname}`.toLowerCase();
  } catch (err) {
    return raw.toLowerCase();
  }
}

function noteGesture(pageUrl, kind) {
  const now = Date.now();
  recentGestures.push({
    url: gestureKey(pageUrl),
    raw: stripHash(pageUrl),
    at: now,
    ttl: kind === 'context' ? CONTEXT_GESTURE_TTL_MS : GESTURE_TTL_MS
  });
  // 只留最近的几十条，别让一个自动点击的页面把内存撑起来。
  while (recentGestures.length > 40) recentGestures.shift();
}

/// 这个下载有没有用户手势。
///
/// - 有 referrer：必须是同一个页面上的点击/右键；
/// - 没有 referrer：地址栏输入、直接导航、浏览器 UI 发起的下载，
///   这类本来就没有页面手势，放行。
function hasUserGesture(referrer) {
  const ref = stripHash(referrer);
  if (!ref) return true;
  const key = gestureKey(ref);

  const now = Date.now();
  for (let i = recentGestures.length - 1; i >= 0; i--) {
    const g = recentGestures[i];
    if (now - g.at > g.ttl) continue;
    if (g.url === key || g.raw === ref) return true;
  }
  return false;
}

/// 浏览器恢复的旧下载。
///
/// Chromium 重启后会把 `state=in_progress` 的下载重新创建一遍，
/// `startTime` 保留的是原始时间。事故里 7 个任务全是 3~5 天前的记录，
/// 却以"新下载"的形式出现在 `onCreated` 里。
function isRestoredDownload(item) {
  const started = Date.parse(item && item.startTime ? item.startTime : '');
  if (!Number.isFinite(started)) return false;
  return Date.now() - started > RESTORED_AGE_MS;
}

/// 取消并清掉浏览器自己恢复的旧下载。
///
/// 只处理 0 字节的：这种记录每次浏览器启动都会被重新拉起来，
/// 不清掉就会一直复活。已经下了部分字节的交给浏览器，别动用户数据。
async function dropRestored(item) {
  try {
    await chrome.downloads.cancel(item.id);
  } catch (err) {
    // 下载可能已经结束
  }
  try {
    await chrome.downloads.erase({ id: item.id });
  } catch (err) {
    // 老版本浏览器没有 erase，或者条目已经没了
  }
  console.info(
    '[IDM Shadow] 已忽略浏览器恢复的旧下载:',
    item.url,
    item.startTime
  );
}

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
//
// `source` 会原样写进任务库。只有 `user` 才允许弹保存位置确认框——
// 页面自动发起的、浏览器恢复的下载没有资格打断用户。
async function capture(item, source = SOURCE.USER) {
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
    source,
    // 让应用弹保存位置确认框（IDM 式流程）。只对用户点击生效。
    needs_confirm: source === SOURCE.USER
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
    // 1. 浏览器恢复的旧下载。
    //
    // 这不是用户此刻发起的：浏览器重启时会把上次没下完的条目
    // 重新创建一遍，历史记录里的 startTime 还是几天前。
    // 事故就是这么来的——7 个旧下载同时复活，7 个确认框一起弹。
    //
    // 不接管。0 字节的连历史一起清掉，免得每次启动都复活；
    // 已经下了部分字节的留给浏览器，不动用户数据。
    if (isRestoredDownload(item)) {
      if ((item.bytesReceived || 0) === 0) {
        await dropRestored(item);
      }
      return;
    }

    // 2. 页面自己发起的下载。
    //
    // onCreated 不区分"用户点了下载"和"页面脚本自己下载"。
    // 内容脚本在真实点击时记一笔手势，这里只认带手势的。
    // 没有手势就交给浏览器：不投任务，也不取消，页面自己的行为
    // 不该由我们来打断。
    if (!hasUserGesture(item.referrer || item.referer)) {
      console.info('[IDM Shadow] 页面自动发起的下载，交给浏览器:', item.url);
      return;
    }

    // 过滤掉的（比如页面预览图）不接管。
    // 也不取消浏览器的下载——让它自己下下去，我们不打扰用户。
    if (await shouldSkip(item)) {
      return;
    }
    const ok = await capture(item, SOURCE.USER);
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
  injectActiveTabs();
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

// 扩展更新/重载之后，已经打开的页面里没有内容脚本了。
//
// Manifest 里的 content_scripts 只在新导航时注入。用户不会为了
// 一次扩展更新去刷新所有标签页，而这些页面里的点击恰恰需要
// 手势消息才能被认成"用户主动下载"。所以把当前活动标签页补一针，
// 之后切换标签页时也各补一针。页面里已有脚本时它会自己跳过。
const injectedTabs = new Set();

async function injectContentInto(tabId) {
  if (!chrome.scripting || !chrome.scripting.executeScript) return;
  if (!tabId || injectedTabs.has(tabId)) return;
  injectedTabs.add(tabId);
  try {
    await chrome.scripting.executeScript({
      target: { tabId },
      files: ['content.js']
    });
  } catch (err) {
    // chrome:// / edge:// 页面、还没加载完的标签页注入不了，忽略。
    injectedTabs.delete(tabId);
  }
}

async function injectActiveTabs() {
  if (!chrome.tabs || !chrome.tabs.query) return;
  try {
    const tabs = await chrome.tabs.query({ active: true });
    for (const tab of tabs) {
      await injectContentInto(tab.id);
    }
  } catch (err) {
    // 拿不到标签页就算了，新导航时 manifest 会注入
  }
}

if (chrome.tabs && chrome.tabs.onActivated) {
  chrome.tabs.onActivated.addListener(({ tabId }) => {
    injectContentInto(tabId);
  });
}

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message && message.type === 'user-gesture') {
    // 内容脚本在真实点击/右键时发来的。页面自己 dispatch 的
    // click 事件 `isTrusted` 是 false，内容脚本那边已经挡住了。
    noteGesture(
      message.pageUrl || (sender && sender.url) || (sender && sender.tab && sender.tab.url),
      message.kind
    );
    sendResponse({ ok: true });
    return false;
  }
  if (message && message.type === 'capture-url') {
    // 只接受内容脚本在用户点击时发来的请求。
    //
    // 老内容脚本不带 source，一律拒绝：它分不清点击和页面自动下载，
    // 放进来就等于把刚堵上的口子又打开了。拒绝之后内容脚本会让
    // 浏览器自己下载，不会丢文件。
    if (message.source !== SOURCE.USER) {
      sendResponse({ ok: false });
      return false;
    }
    // 内容脚本发来的接管请求也要守模式。
    //
    // 用户在设置里选了"不接管"，内容脚本却照样把下载抢走的话，
    // 那个开关就形同虚设了。
    settings()
      .then((s) => {
        if (effectiveMode(s) === 'off') return false;
        return capture(
          {
            url: message.url,
            filename: message.filename || '',
            referer: message.referer
          },
          SOURCE.USER
        );
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
injectActiveTabs();
