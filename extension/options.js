const $ = (id) => document.getElementById(id);

// 和后台脚本一样的发现逻辑：依次试几个端口，问应用要地址和 token。
const CANDIDATE_PORTS = [16888, 16889, 16890, 16891, 16892, 16893, 16894, 16895];

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

(async function init() {
  const stored = await chrome.storage.local.get({
    bridgeUrl: 'http://127.0.0.1:16888',
    token: '',
    // 默认直接接管：浏览器点下载就弹 IDM Shadow 自己的对话框，
    // 不走浏览器的通知气泡。桌面应用里的「设为默认下载工具」
    // 开关要用的话，装好扩展后切到「跟随应用设置」就行。
    mode: 'auto',
    autoLaunch: true
  });
  // 老版本存过 'ask'（先弹浏览器通知气泡再接管）。
  // 现在没有那个气泡了，直接按"接管"处理。
  if (stored.mode === 'ask') stored.mode = 'auto';
  $('bridgeUrl').value = stored.bridgeUrl;
  $('token').value = stored.token;
  $('mode').value = stored.mode;
  $('autoLaunch').checked = stored.autoLaunch;

  // 已经配好了就直接显示状态，不去打扰应用
  if (stored.token) {
    $('autoMsg').textContent = '已配置';
  }
})();

$('auto').addEventListener('click', async () => {
  $('autoMsg').textContent = '正在找…';
  const found = await discoverApp();
  if (!found) {
    $('autoMsg').textContent = '没找到应用，确认它已经启动';
    return;
  }
  await chrome.storage.local.set({
    bridgeUrl: found.bridgeUrl,
    token: found.token
  });
  $('bridgeUrl').value = found.bridgeUrl;
  $('token').value = found.token;
  $('autoMsg').textContent = '配置好了';
});

$('toggle').addEventListener('click', () => {
  const input = $('token');
  const showing = input.type === 'text';
  input.type = showing ? 'password' : 'text';
  $('toggle').textContent = showing ? '显示' : '隐藏';
});

$('save').addEventListener('click', async () => {
  await chrome.storage.local.set({
    bridgeUrl: $('bridgeUrl').value.trim().replace(/\/$/, ''),
    token: $('token').value.trim(),
    mode: $('mode').value,
    autoLaunch: $('autoLaunch').checked
  });
  $('saved').textContent = '已保存';
  setTimeout(() => ($('saved').textContent = ''), 2000);
});
