// 页面内的「发送到 IDM Shadow」入口。
//
// 三件事：
//  1. 在媒体链接旁边挂一个按钮，把当前页面的视频源交给后台；
//  2. 接管跨源的 a[download] 点击——那种情况浏览器当成页面导航，
//     chrome.downloads.onCreated 根本不触发，扩展从头到尾不会被唤醒，
//     用户点下载就是一点反应都没有；
//  3. 把结果告诉后台。

(function () {
  'use strict';

  if (window.__idmShadowInjected) return;
  window.__idmShadowInjected = true;

  // 扩展上下文还有效吗？
  //
  // 扩展更新/重载之后，页面里跑的这份脚本就变成了"孤儿"：
  // 监听器还挂着，但 chrome.runtime.sendMessage 已经失效，
  // 发出去的消息没人接。而它拦住跨源 a[download] 那一步用了
  // preventDefault()——消息发不出去，点击就彻底没反应。
  //
  // 后台靠这个标记判断要不要往页面里补一份新的。
  function contextAlive() {
    try {
      // 扩展被停用/重载之后，sendMessage 会失败。
      //
      // 不能只看 chrome.runtime.id——重载前后 id 是一样的。
      // 真正会变的是 sendMessage 的回调：上下文还在时它会带响应，
      // 上下文没了就抛 "Extension context invalidated"。
      //
      // 这里用一次真实的往返来判断。代价是几十秒一次，很轻。
      return new Promise((resolve) => {
        try {
          chrome.runtime.sendMessage(
            { type: 'content-alive' },
            (response) => {
              void chrome.runtime.lastError;
              resolve(Boolean(response && response.alive));
            }
          );
        } catch (err) {
          resolve(false);
        }
      });
    } catch (err) {
      return Promise.resolve(false);
    }
  }

  // 只有顶层页面才挂「↓ IDM」按钮。
  //
  // 内容脚本在所有框架里都跑（iframe 里的点击不会冒泡到顶层，
  // 手势消息会丢），但按钮每个框架都挂一份就到处都是了。
  const isTop = window.top === window;

  const MEDIA_RE = /\.(m3u8|mp4|mkv|webm|flv|ts)(\?|#|$)/i;

  // 是不是跨源的。
  function isCrossOrigin(url) {
    try {
      return new URL(url, location.href).origin !== location.origin;
    } catch (err) {
      return false;
    }
  }

  // 浏览器对 blob:/data: 的处理跟普通链接不一样，别碰。
  function isUnsupported(url) {
    return /^(blob|data|javascript|chrome-extension|edge):/i.test(url);
  }

  function send(url, filename) {
    chrome.runtime.sendMessage(
      {
        type: 'capture-url',
        url: url,
        filename: filename || '',
        referer: location.href,
        // 这是用户点了按钮才发出来的。后台只认带这个标记的请求，
        // 页面脚本没法伪造（内容脚本在隔离世界里）。
        source: 'user'
      },
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
    if (!isTop) return;
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

  // 接管跨源的 a[download] 点击。
  //
  // 为什么需要这一段：
  // 浏览器对 a[download] 的处理分两种。目标是同源（或者最终响应带
  // Content-Disposition: attachment）时，它会触发 chrome.downloads.onCreated，
  // 扩展能接管。但目标是跨源、且最终响应没有 disposition 时，浏览器把
  // 它当成**页面导航**——于是没有下载、没有 onCreated、扩展全程没醒，
  // 用户点了下载就是一点反应都没有。
  //
  // 所以这里在捕获阶段先拦下来，交给后台去下载。
  // 拦不住或者后台失败，就让浏览器按原来的方式走（见下面的兜底）。
  function installDownloadInterceptor() {
    // 记下来，扩展重载后要能摘掉（见 __idmShadowCleanup）。
    onDownloadClick = (event) => {
        // 只认真实点击。
        //
        // `isTrusted` 为 false 说明这个事件是页面脚本用
        // dispatchEvent 造出来的——那不是用户点的。放进来等于
        // 给页面留了一个"假装用户下载"的后门。
        if (!event.isTrusted) return;
        // 只处理左键单击。右键菜单另说（浏览器自己会弹"下载"菜单项）。
        if (event.defaultPrevented) return;
        if (event.button !== 0) return;
        if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;

        // 只认带 download 属性的链接。
        //
        // 这是精准信号：页面明确说了"这是个下载"。
        // 普通外链（a[href]）不碰——那是浏览器打开新页面的事，
        // 拦下来会把"点开看 PDF"变成"直接下载"，用户会觉得莫名其妙。
        const anchor =
          event.target && event.target.closest
            ? event.target.closest('a[download]')
            : null;
        if (!anchor) return;

        const href = anchor.getAttribute('href');
        if (!href) return;

        let resolved;
        try {
          resolved = new URL(href, location.href).href;
        } catch (err) {
          return;
        }
        if (isUnsupported(resolved)) return;
        // 同源的不用管——那种情况 onCreated 会触发，扩展接得住。
        if (!isCrossOrigin(resolved)) return;

        // download 属性里的文件名是页面给的，比浏览器猜的 "download" 可靠。
        const attr = anchor.getAttribute('download');
        const name = attr && attr.trim() ? attr.trim() : '';

        event.preventDefault();
        event.stopPropagation();
        chrome.runtime.sendMessage(
          {
            type: 'capture-url',
            url: resolved,
            filename: name,
            referer: location.href,
            source: 'user'
          },
          (response) => {
            if (chrome.runtime.lastError) {
              // 后台不在（扩展刚更新还没重载？）——放回给浏览器。
              fallbackDownload(resolved, name);
              return;
            }
            if (!response || !response.ok) {
              // 应用没接住。别把下载弄丢，让浏览器自己下。
              fallbackDownload(resolved, name);
            }
          }
        );
    };
    document.addEventListener('click', onDownloadClick, true);
  }

  // 接管失败时的兜底：让浏览器自己下。
  //
  // 优先用 chrome.downloads.download（它能带上文件名）；
  // 扩展刚装上、还没拿到 downloads 权限之外的场景就用普通导航。
  function fallbackDownload(url, filename) {
    try {
      chrome.downloads.download(
        { url, filename: filename || undefined },
        () => {
          void chrome.runtime.lastError;
        }
      );
    } catch (err) {
      // 实在不行就让浏览器自己导航过去。
      window.location.href = url;
    }
  }

  // 跨源 a[download] 的拦截器。函数体在下面赋值，这里先声明，
  // 好让 __idmShadowCleanup 能把它摘掉。
  let onDownloadClick = null;

  // 把用户手势报给后台。
  //
  // `downloads.onCreated` 分不清"用户点了下载"和"页面自己下载"。
  // 真实点击的 `isTrusted` 是 true；页面脚本 dispatch 的点击是 false，
  // 这里只报前者。后台据此决定要不要接管。
  let lastGestureAt = 0;
  function reportGesture(kind) {
    const now = Date.now();
    // 连点几下不用每一下都发消息。
    if (now - lastGestureAt < 400) return;
    lastGestureAt = now;
    try {
      chrome.runtime.sendMessage(
        { type: 'user-gesture', pageUrl: location.href, kind: kind },
        () => {
          // 后台可能刚重载，读一下 lastError 免得控制台报
          // "Unchecked runtime.lastError"。
          void chrome.runtime.lastError;
        }
      );
    } catch (err) {
      // 扩展上下文失效（刚更新/重载），等页面刷新后重新注入
    }
  }

  // 监听器都记下来，扩展重载后要能把自己摘掉。
  const onClick = (event) => {
    if (event.isTrusted) reportGesture('click');
  };
  const onContextMenu = (event) => {
    if (event.isTrusted) reportGesture('context');
  };

  const timer = setInterval(scan, 1500);
  setTimeout(() => clearInterval(timer), 60000);
  scan();
  installDownloadInterceptor();

  document.addEventListener('click', onClick, true);
  document.addEventListener('contextmenu', onContextMenu, true);

  // 后台在补新脚本之前会调这个，把旧脚本的监听器和定时器摘掉。
  //
  // 不摘的话新旧两个脚本会同时拦点击：新脚本能正常工作，但旧脚本
  // 那个失效的 sendMessage 回调会先 preventDefault，消息发不出去。
  window.__idmShadowCleanup = function () {
    clearInterval(timer);
    document.removeEventListener('click', onClick, true);
    document.removeEventListener('contextmenu', onContextMenu, true);
    document.removeEventListener('click', onDownloadClick, true);
    window.__idmShadowInjected = false;
    window.__idmShadowAlive = false;
  };

  // 活着标记。后台靠它判断页面里这份脚本还能用。
  //
  // 定期更新：扩展被停用/重载之后 sendMessage 就发不出去了，
  // 这时把标记翻成 false，后台就知道该往页面里补一份新的。
  contextAlive().then((alive) => {
    window.__idmShadowAlive = alive;
  });
  setInterval(() => {
    contextAlive().then((alive) => {
      window.__idmShadowAlive = alive;
    });
  }, 30000);
})();
