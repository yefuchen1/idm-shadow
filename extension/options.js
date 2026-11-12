const $ = (id) => document.getElementById(id);

(async function init() {
  const stored = await chrome.storage.local.get({
    bridgeUrl: 'http://127.0.0.1:16888',
    token: '',
    mode: 'ask',
    autoLaunch: true
  });
  $('bridgeUrl').value = stored.bridgeUrl;
  $('token').value = stored.token;
  $('mode').value = stored.mode;
  $('autoLaunch').checked = stored.autoLaunch;
})();

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
