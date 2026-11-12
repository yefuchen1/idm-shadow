// 页面内的「发送到 IDM Shadow」入口。
// 只做两件事：在媒体链接旁边挂一个按钮，把当前页面的视频源交给后台。

(function () {
  'use strict';

  if (window.__idmShadowInjected) return;
  window.__idmShadowInjected = true;

  const MEDIA_RE = /\.(m3u8|mp4|mkv|webm|flv|ts)(\?|#|$)/i;

  function send(url, filename) {
    chrome.runtime.sendMessage(
      { type: 'capture-url', url: url, filename: filename || '', referer: location.href },
      (response) => {
        if (chrome.runtime.lastError) {
          console.warn('[IDM Shadow] 发送失败', chrome.runtime.lastError);
        } else if (response && !response.ok) {
          console.warn('[IDM Shadow] 应用未接受任务');
        }
      }
    );
  }

  function button(label, title) {
    const el = document.createElement('button');
    el.type = 'button';
    el.textContent = label;
    el.title = title;
    el.style.cssText =
      'margin-left:6px;padding:1px 6px;font-size:12px;line-height:1.4;cursor:pointer;' +
      'border:1px solid #c8cdd4;border-radius:4px;background:#f3f5f7;color:#333;';
    el.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      const url = el.dataset.url;
      send(url, el.dataset.filename);
      const original = el.textContent;
      el.textContent = '已发送';
      setTimeout(() => (el.textContent = original), 1200);
    });
    return el;
  }

  function scan() {
    document.querySelectorAll('a[href]').forEach((anchor) => {
      if (anchor.dataset.idmShadowDone) return;
      const href = anchor.getAttribute('href') || '';
      if (!MEDIA_RE.test(href)) return;
      anchor.dataset.idmShadowDone = '1';
      const url = new URL(href, location.href).href;
      const btn = button('↓ IDM', '用 IDM Shadow 下载');
      btn.dataset.url = url;
      btn.dataset.filename = anchor.textContent || '';
      anchor.insertAdjacentElement('afterend', btn);
    });
  }

  const timer = setInterval(scan, 1500);
  setTimeout(() => clearInterval(timer), 60000);
  scan();
})();
