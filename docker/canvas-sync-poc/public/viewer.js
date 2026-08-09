'use strict';

const canvas = document.getElementById('canvas');
const ctx = canvas.getContext('2d', { alpha: false });
const statusEl = document.getElementById('status');
const urlEl = document.getElementById('url');
const fpsEl = document.getElementById('fps');
const cursorEl = document.getElementById('cursor');

// 最近一帧的 metadata,反向坐标映射要用
let meta = { deviceWidth: 1280, deviceHeight: 800, pageScaleFactor: 1, offsetTop: 0 };
let frameCount = 0;
let lastFpsTs = performance.now();

let ws = null;
let reconnectTimer = null;

function send(o) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(o));
}

function onMessage(ev) {
  // 控制消息(JSON)
  if (typeof ev.data === 'string') {
    let m;
    try { m = JSON.parse(ev.data); } catch { return; }
    if (m.type === 'meta') {
      meta = m;
      if (canvas.width !== m.deviceWidth) canvas.width = m.deviceWidth;
      if (canvas.height !== m.deviceHeight) canvas.height = m.deviceHeight;
      if (m.url) urlEl.textContent = m.url;
    } else if (m.type === 'navigate') {
      if (m.url) urlEl.textContent = m.url;
    }
    return;
  }
  // 画面帧(binary JPEG)
  createImageBitmap(new Blob([ev.data], { type: 'image/jpeg' }))
    .then(bmp => {
      ctx.drawImage(bmp, 0, 0, canvas.width, canvas.height);
      bmp.close();
    })
    .catch(() => {});
  frameCount++;
  const now = performance.now();
  if (now - lastFpsTs >= 1000) {
    fpsEl.textContent = Math.round((frameCount * 1000) / (now - lastFpsTs)) + ' fps';
    frameCount = 0;
    lastFpsTs = now;
  }
}

// 自动重连:WS 断开 1s 后自动重连(配合 relay 每连接重启 screencast,根治黑屏)
function connect() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  statusEl.textContent = 'connecting';
  statusEl.className = 'badge connecting';
  ws = new WebSocket((location.protocol === 'https:' ? 'wss' : 'ws') + '://' + location.host + '/ws');
  ws.binaryType = 'arraybuffer';

  ws.onopen = () => {
    statusEl.textContent = 'open';
    statusEl.className = 'badge open';
    canvas.focus();
  };
  ws.onmessage = onMessage;
  ws.onclose = () => {
    statusEl.textContent = 'reconnecting';
    statusEl.className = 'badge closed';
    if (!reconnectTimer) reconnectTimer = setTimeout(connect, 1000);
  };
  ws.onerror = () => {}; // 交由 onclose 处理重连
}

connect();

// ---- 反向输入捕获 ----
function mods(e) {
  return (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
}

function map(e) {
  const rect = canvas.getBoundingClientRect();
  const x = ((e.clientX - rect.left) / rect.width) * meta.deviceWidth;
  const y = ((e.clientY - rect.top) / rect.height) * meta.deviceHeight;
  return { x, y };
}

// pointermove 节流(16ms ≈ 60fps)
let lastMoveTs = 0;
canvas.addEventListener('pointermove', e => {
  cursorEl.style.display = 'block';
  cursorEl.style.left = e.clientX + 'px';
  cursorEl.style.top = e.clientY + 'px';
  const now = performance.now();
  if (now - lastMoveTs < 16) return;
  lastMoveTs = now;
  const { x, y } = map(e);
  send({ t: 'mm', x, y, mods: mods(e) });
});

canvas.addEventListener('pointerdown', e => {
  e.preventDefault();
  canvas.focus();
  const { x, y } = map(e);
  send({ t: 'mp', x, y, btn: 'left', count: 1, mods: mods(e) });
});

canvas.addEventListener('pointerup', e => {
  e.preventDefault();
  const { x, y } = map(e);
  send({ t: 'mr', x, y, btn: 'left', count: 1, mods: mods(e) });
});

canvas.addEventListener('wheel', e => {
  e.preventDefault();
  const { x, y } = map(e);
  send({ t: 'wh', x, y, dx: e.deltaX, dy: e.deltaY, mods: mods(e) });
}, { passive: false });

canvas.addEventListener('contextmenu', e => e.preventDefault());

// 键盘:可打印字符走 insertText,控制键走 keyDown
canvas.addEventListener('keydown', e => {
  e.preventDefault();
  if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) {
    send({ t: 'ti', text: e.key });
  } else {
    send({ t: 'kd', key: e.key, code: e.code, mods: mods(e) });
  }
});

canvas.addEventListener('keyup', e => {
  e.preventDefault();
  if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) return;
  send({ t: 'ku', key: e.key, code: e.code, mods: mods(e) });
});
