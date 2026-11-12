// ==UserScript==
// @name         IDM Shadow 桥接
// @namespace    idm-shadow
// @version      0.1.0
// @description  把油猴脚本触发的下载交给 IDM Shadow 应用接管。
// @author       IDM Shadow
// @match        *://*/*
// @grant        GM_xmlhttpRequest
// @grant        GM_setValue
// @grant        GM_getValue
// @grant        GM_registerMenuCommand
// @connect      127.0.0.1
// @connect      localhost
// @run-at       document-idle
// @noframes
// ==/UserScript==

(function () {
  'use strict';

  const DEFAULTS = {
    bridgeUrl: 'http://127.0.0.1:16888',
    token: '',
    autoStart: true,
    injectButton: true
  };

  function loadSettings() {
    const stored = GM_getValue('settings', {});
    return Object.assign({}, DEFAULTS, stored || {});
  }

  function saveSettings(patch) {
    const next = Object.assign({}, loadSettings(), patch);
    GM_setValue('settings', next);
    return next;
  }

  function request(method, path, body) {
    const s = loadSettings();
    return new Promise((resolve, reject) => {
      GM_xmlhttpRequest({
        method: method,
        url: s.bridgeUrl + path,
        headers: {
          'Content-Type': 'application/json',
          'X-IDM-Shadow-Token': s.token
        },
        data: body ? JSON.stringify(body) : undefined,
        timeout: 8000,
        onload: (resp) => {
          if (resp.status >= 200 && resp.status < 300) {
            try {
              resolve(JSON.parse(resp.responseText));
            } catch (err) {
              resolve(resp.responseText);
            }
          } else {
            let message = 'HTTP ' + resp.status;
            try {
              const parsed = JSON.parse(resp.responseText);
              if (parsed && parsed.error && parsed.error.message) message = parsed.error.message;
            } catch (err) {
              // 保留状态码
            }
            reject(new Error(message));
          }
        },
        onerror: () => reject(new Error('无法连接 IDM Shadow')),
        ontimeout: () => reject(new Error('连接 IDM Shadow 超时'))
      });
    });
  }

  // 应用没开的时候，用自定义协议唤起它。
  function launchViaProtocol(url, name) {
    const params = new URLSearchParams();
    if (url) params.set('url', url);
    if (name) params.set('name', name);
    const target = params.toString() ? 'idmshadow://dl?' + params.toString() : 'idmshadow://';
    window.location.href = target;
  }

  const api = {
    settings: loadSettings,

    async alive() {
      try {
        await request('GET', '/api/v1/health');
        return true;
      } catch (err) {
        return false;
      }
    },

    /**
     * 交一个下载任务给 IDM Shadow。
     * @param {string} url 直链
     * @param {object} opts { filename, referer, cookies, engine, headers }
     */
    async download(url, opts) {
      opts = opts || {};
      const payload = {
        url: url,
        filename: opts.filename,
        referer: opts.referer || document.location.href,
        userAgent: opts.userAgent || navigator.userAgent,
        cookies: opts.cookies || document.cookie || undefined,
        headers: opts.headers || {},
        engine: opts.engine || 'auto',
        maxConnections: opts.maxConnections,
        autostart: true
      };

      try {
        const result = await request('POST', '/api/v1/tasks', payload);
        return result;
      } catch (err) {
        const s = loadSettings();
        if (s.autoStart) {
          launchViaProtocol(url, opts.filename);
          return { id: null, launched: true };
        }
        throw err;
      }
    },

    async list() {
      const result = await request('GET', '/api/v1/tasks');
      return result.tasks || [];
    },

    async configure() {
      const s = loadSettings();
      const bridgeUrl = window.prompt('IDM Shadow bridge 地址', s.bridgeUrl);
      if (bridgeUrl === null) return;
      const token = window.prompt('IDM Shadow token', s.token);
      if (token === null) return;
      saveSettings({ bridgeUrl: bridgeUrl.trim(), token: token.trim() });
      const alive = await api.alive();
      window.alert(alive ? '已连接 IDM Shadow' : '保存成功，但连不上应用');
    }
  };

  window.IDMShadow = api;

  // 尝试接管 GM_download：多数油猴脚本的下载按钮走的就是它。
  // 接管失败也没关系——浏览器下载栈会被扩展拦下。
  try {
    const original = window.GM_download;
    if (typeof original === 'function') {
      Object.defineProperty(window, 'GM_download', {
        configurable: true,
        writable: true,
        value: function (url, name) {
          api.download(url, { filename: name }).catch((err) => {
            console.warn('[IDM Shadow] 投递失败', err);
          });
        }
      });
    }
  } catch (err) {
    console.debug('[IDM Shadow] 无法覆盖 GM_download，改由扩展接管');
  }

  // 页面内媒体按钮
  const MEDIA_RE = /\.(m3u8|mp4|mkv|webm|flv|ts|zip|rar|7z|apk|exe|iso|pdf)(\?|#|$)/i;

  function isMedia(url) {
    return typeof url === 'string' && MEDIA_RE.test(url);
  }

  function injectButton(anchor, url) {
    if (anchor.dataset.idmShadowDone) return;
    anchor.dataset.idmShadowDone = '1';
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = '↓ IDM';
    button.title = '用 IDM Shadow 下载';
    button.style.cssText =
      'margin-left:6px;padding:1px 6px;font-size:12px;line-height:1.4;' +
      'cursor:pointer;border:1px solid #c8cdd4;border-radius:4px;background:#f3f5f7;color:#333;';
    button.addEventListener('click', (event) => {
      event.preventDefault();
      event.stopPropagation();
      api.download(url, { referer: document.location.href }).then(
        () => {
          button.textContent = '✓';
          setTimeout(() => (button.textContent = '↓ IDM'), 1500);
        },
        (err) => {
          button.textContent = '失败';
          console.warn('[IDM Shadow]', err);
          setTimeout(() => (button.textContent = '↓ IDM'), 1500);
        }
      );
    });
    anchor.insertAdjacentElement('afterend', button);
  }

  function scan() {
    if (!loadSettings().injectButton) return;
    document.querySelectorAll('a[href]').forEach((anchor) => {
      const href = anchor.getAttribute('href');
      if (isMedia(href)) injectButton(anchor, new URL(href, location.href).href);
    });
    document.querySelectorAll('video[src], audio[src], video source[src], source[src]').forEach((el) => {
      const src = el.getAttribute('src');
      if (!src || !isMedia(src)) return;
      if (el.dataset.idmShadowDone) return;
      el.dataset.idmShadowDone = '1';
      const url = new URL(src, location.href).href;
      const wrapper = document.createElement('span');
      wrapper.style.cssText = 'display:inline-block;margin-left:8px;';
      el.parentNode.insertBefore(wrapper, el.nextSibling);
      injectButton(wrapper, url);
    });
  }

  const observer = new MutationObserver(() => {
    clearTimeout(scan._timer);
    scan._timer = setTimeout(scan, 400);
  });

  function start() {
    scan();
    observer.observe(document.documentElement, { childList: true, subtree: true });
    if (typeof GM_registerMenuCommand === 'function') {
      GM_registerMenuCommand('IDM Shadow 设置', () => api.configure());
      GM_registerMenuCommand('用 IDM Shadow 下载当前视频', () => {
        const video = document.querySelector('video[src], video source[src]');
        const url = video && (video.getAttribute('src') || video.currentSrc);
        if (url) api.download(url, { referer: location.href });
      });
    }
  }

  if (document.readyState === 'complete') start();
  else window.addEventListener('load', start);
})();
