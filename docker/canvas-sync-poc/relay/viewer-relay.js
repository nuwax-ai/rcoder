'use strict';

/**
 * viewer-relay: Canvas 双向同步 POC 的中继服务
 *
 * 正向: CDP Page.startScreencast → JPEG 帧(binary WS)+ meta/navigate(JSON WS)
 * 反向: 客户端输入(JSON WS)→ CDP Input.dispatchMouseEvent / dispatchKeyEvent / insertText
 *
 * 端点:
 *   GET  /                      viewer 页面(public/index.html)
 *   GET  /viewer.js             viewer 脚本
 *   GET  /test-page[/index.html] 容器内 Chrome 加载的测试页
 *   WS   /ws                    双向通道
 *   GET  /agent/click?x=&y=     模拟 agent 点击(验证「agent 操作被 viewer 看到」)
 */

const http = require('http');
const fs = require('fs');
const path = require('path');
const url = require('url');
const puppeteer = require('puppeteer-core');
const { WebSocketServer } = require('ws');

const PORT = parseInt(process.env.PORT || '9223', 10);
const HOST = '0.0.0.0';
const CHROME_URL = process.env.CHROME_URL || 'http://127.0.0.1:9222';
const TEST_PAGE_URL = `http://127.0.0.1:${PORT}/test-page/index.html`;

const PUBLIC_DIR = path.join(__dirname, '..', 'public');
const TESTPAGE_DIR = path.join(__dirname, '..', 'test-page');

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'application/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
};

function sendFile(res, filePath) {
  fs.readFile(filePath, (err, data) => {
    if (err) { res.writeHead(404); res.end('not found'); return; }
    res.writeHead(200, { 'Content-Type': MIME[path.extname(filePath)] || 'application/octet-stream' });
    res.end(data);
  });
}

// ---- Chrome / CDP 单实例状态(所有 WS 客户端共享同一个 Chrome page)----
let browser;
let activePage;
let cdp;
const subscribers = new Set(); // 已连接的 WS 客户端

async function attachChrome() {
  browser = await puppeteer.connect({ browserURL: CHROME_URL, defaultViewport: null });
  const pages = await browser.pages();
  activePage = pages[0] || (await browser.newPage());
  await activePage.setViewport({ width: 1280, height: 800 });
  cdp = await activePage.target().createCDPSession();
  await cdp.send('Page.enable');

  // 正向:每帧广播给所有订阅者,然后 ack(不 ack Chrome 会停推)
  cdp.on('Page.screencastFrame', async ({ data, metadata, sessionId }) => {
    const meta = JSON.stringify({
      type: 'meta',
      deviceWidth: metadata.deviceWidth,
      deviceHeight: metadata.deviceHeight,
      pageScaleFactor: metadata.pageScaleFactor,
      offsetTop: metadata.offsetTop,
      url: activePage.url(),
    });
    const buf = Buffer.from(data, 'base64');
    for (const ws of subscribers) {
      if (ws.readyState === ws.OPEN) {
        ws.send(meta);
        ws.send(buf);
      }
    }
    try {
      await cdp.send('Page.screencastFrameAck', { sessionId });
    } catch {
      // 页面导航中 ack 可能失败,忽略
    }
  });

  cdp.on('Page.frameNavigated', ({ frame }) => {
    if (frame.parentId) return; // 只关心顶层框架
    const msg = JSON.stringify({ type: 'navigate', url: frame.url });
    for (const ws of subscribers) {
      if (ws.readyState === ws.OPEN) ws.send(msg);
    }
  });

  // 注意:不在这里 goto 测试页 —— 此时 HTTP server 还没 listen,Chrome 连不上 9223。
  // goto 放到 server.listen 回调里执行。
}

// 每次 WS 连接都强制重启推流:Chrome 的 screencast 会在 page 导航 / crash / 长时间静止后
// 停止,这里 stop + start 确保新连接一定能收到帧(根治 viewer 黑屏)。
async function restartScreencast() {
  try {
    await cdp.send('Page.stopScreencast');
  } catch {
    // 首次启动或已停止,忽略
  }
  await cdp.send('Page.startScreencast', {
    format: 'jpeg',
    quality: 65,
    maxWidth: 1280,
    maxHeight: 800,
  });
}

// 反向:把客户端输入翻译成 CDP Input 命令
async function handleInput(msg) {
  const mods = msg.mods || 0;
  switch (msg.t) {
    case 'mm':
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: msg.x, y: msg.y, modifiers: mods });
      break;
    case 'mp':
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: msg.x, y: msg.y, button: msg.btn || 'left', clickCount: msg.count || 1, buttons: 1, modifiers: mods });
      break;
    case 'mr':
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: msg.x, y: msg.y, button: msg.btn || 'left', clickCount: msg.count || 1, buttons: 0, modifiers: mods });
      break;
    case 'wh':
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: msg.x, y: msg.y, deltaX: msg.dx || 0, deltaY: msg.dy || 0, modifiers: mods });
      break;
    case 'kd':
      await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: msg.key, code: msg.code, modifiers: mods });
      break;
    case 'ku':
      await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: msg.key, code: msg.code, modifiers: mods });
      break;
    case 'ti':
      await cdp.send('Input.insertText', { text: String(msg.text || '') });
      break;
    default:
      break;
  }
}

// ---- HTTP server ----
const server = http.createServer((req, res) => {
  const parsed = url.parse(req.url, true);
  const pathname = decodeURIComponent(parsed.pathname);

  if (pathname === '/agent/click') {
    const x = parseFloat(parsed.query.x);
    const y = parseFloat(parsed.query.y);
    if (Number.isNaN(x) || Number.isNaN(y)) {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'need numeric x,y query' }));
      return;
    }
    handleInput({ t: 'mp', x, y, btn: 'left', count: 1 })
      .then(() => handleInput({ t: 'mr', x, y, btn: 'left', count: 1 }))
      .then(() => {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ ok: true, x, y, url: activePage.url() }));
      })
      .catch(e => {
        res.writeHead(500, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ error: e.message }));
      });
    return;
  }

  if (pathname === '/' || pathname === '/index.html') return sendFile(res, path.join(PUBLIC_DIR, 'index.html'));
  if (pathname === '/viewer.js') return sendFile(res, path.join(PUBLIC_DIR, 'viewer.js'));

  if (pathname === '/test-page' || pathname === '/test-page/' || pathname === '/test-page/index.html') {
    return sendFile(res, path.join(TESTPAGE_DIR, 'index.html'));
  }

  res.writeHead(404);
  res.end('not found');
});

// ---- WebSocket server ----
const wss = new WebSocketServer({ server, path: '/ws' });

wss.on('connection', ws => {
  console.log('[ws] client connected');
  subscribers.add(ws);
  restartScreencast().catch(e => console.error('[ws] startScreencast failed:', e.message));

  ws.on('message', raw => {
    let msg;
    try {
      msg = JSON.parse(raw.toString());
    } catch {
      return;
    }
    handleInput(msg).catch(e => console.error('[input]', msg.t, e.message));
  });
  ws.on('close', () => {
    console.log('[ws] client disconnected');
    subscribers.delete(ws);
  });
  ws.on('error', () => {});
});

(async () => {
  console.log('[relay] connecting to chrome at', CHROME_URL);
  await attachChrome();
  console.log('[relay] chrome attached; page url =', activePage.url());
  server.listen(PORT, HOST, () => {
    console.log(`[relay] listening on http://${HOST}:${PORT}`);
    console.log(`[relay]   viewer     : http://<host>:${PORT}/`);
    console.log(`[relay]   websocket  : ws://<host>:${PORT}/ws`);
    console.log(`[relay]   agent click: http://<host>:${PORT}/agent/click?x=&y=`);
    // server 已 listen,Chrome 现在能连上 9223 了,加载测试页
    activePage
      .goto(TEST_PAGE_URL, { waitUntil: 'domcontentloaded' })
      .then(() => console.log('[relay] test page loaded:', activePage.url()))
      .catch(e => console.error('[relay] navigate to test page failed:', e.message));
  });
})().catch(e => {
  console.error('[relay] fatal:', e);
  process.exit(1);
});
