const $ = (id) => document.getElementById(id);

function formatSpeed(bytesPerSec) {
  if (!bytesPerSec) return '—';
  const units = ['B/s', 'KB/s', 'MB/s', 'GB/s'];
  let value = bytesPerSec;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  return `${value.toFixed(value >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatSize(bytes) {
  if (bytes === null || bytes === undefined) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i += 1;
  }
  return `${value.toFixed(value >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
}

const STATUS_LABEL = {
  queued: '排队中',
  probing: '探测中',
  downloading: '下载中',
  retrying: '重试中',
  paused: '已暂停',
  completed: '已完成',
  error: '出错',
  cancelled: '已取消'
};

async function refresh() {
  const status = await chrome.runtime.sendMessage({ type: 'status' });
  const dot = document.querySelector('.dot');
  if (status && status.alive) {
    dot.classList.add('ok');
    $('status').lastChild.textContent = '已连接';
  } else {
    dot.classList.remove('ok');
    $('status').lastChild.textContent = '未连接';
  }

  const body = await chrome.runtime.sendMessage({ type: 'tasks' });
  const tasks = (body && body.tasks) || [];
  const list = $('tasks');
  list.innerHTML = '';

  let active = 0;
  let totalSpeed = 0;
  tasks.forEach((task) => {
    if (['queued', 'probing', 'downloading', 'retrying'].includes(task.status)) {
      active += 1;
      totalSpeed += task.speed || 0;
    }
    const li = document.createElement('li');
    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = task.filename;
    name.title = task.path;
    const right = document.createElement('span');
    right.className = 'speed';
    right.textContent = task.status === 'downloading'
      ? formatSpeed(task.speed)
      : STATUS_LABEL[task.status] || task.status;
    li.appendChild(name);
    li.appendChild(right);
    list.appendChild(li);
  });

  $('summary').textContent = tasks.length
    ? `${active} 个进行中 · 总速度 ${formatSpeed(totalSpeed)}`
    : '暂无任务';
}

$('mode').addEventListener('change', (event) => {
  chrome.storage.local.set({ mode: event.target.value });
});

$('open').addEventListener('click', () => {
  // 让应用把主窗口叫到前面。
  //
  // 以前这里用的是 chrome.tabs.create({ url: 'idmshadow://' })，
  // Edge 会开一个标签页去加载协议链接，然后把
  // 「无法访问此页面 / localhost 拒绝连接」的错误页留在那儿。
  // 走 bridge 接口就没这个问题。
  showAppWindow();
});

async function showAppWindow() {
  const s = await chrome.storage.local.get({
    bridgeUrl: 'http://127.0.0.1:16888',
    token: ''
  });
  if (!s.token) return;
  try {
    await fetch(`${s.bridgeUrl}/api/v1/ui/show`, {
      method: 'POST',
      headers: {
        'X-IDM-Shadow-Token': s.token,
        // 让应用知道扩展还活着（设置页显示连接状态）
        'X-IDM-Shadow-Client': 'extension',
        // 顺便报一下版本：应用要靠它区分"扩展没装"和"跑着旧代码的扩展"。
        'X-IDM-Shadow-Version': (() => {
          try {
            return (chrome.runtime.getManifest() || {}).version || '';
          } catch (err) {
            return '';
          }
        })()
      }
    });
  } catch (err) {
    // 应用没在跑，忽略
  }
}

$('options').addEventListener('click', () => {
  chrome.runtime.openOptionsPage();
});

(async function init() {
  // 默认直接接管，弹 IDM Shadow 自己的下载对话框。
  const stored = await chrome.storage.local.get({ mode: 'auto' });
  // 老版本存过 'ask'，现在没有那个通知气泡了，直接按"接管"处理。
  if (stored.mode === 'ask') stored.mode = 'auto';
  $('mode').value = stored.mode;
  await refresh();
  setInterval(refresh, 1000);
})();
