'use strict';

/**
 * 端到端双向通道验证脚本(容器内运行)
 *
 * 以一个 WS 客户端身份连 relay /ws,自动验证:
 *   - 正向:收到 screencast binary 帧 + meta JSON
 *   - 反向:经 WS 发 click 到计数器按钮 → count +1
 *   - 反向:经 WS 发 click 到弹框 ✕ → modal 隐藏
 *   - 反向:经 WS 发 insertText → input.value 写入
 *
 * DOM 状态通过 puppeteer 直连 9222 读取(只读校验,不绕过 relay 通道)。
 */

const WebSocket = require('ws');
const puppeteer = require('puppeteer-core');

const sleep = ms => new Promise(r => setTimeout(r, ms));

let frameCount = 0;
let gotMeta = false;
const ws = new WebSocket('ws://127.0.0.1:9223/ws');

ws.on('message', (data, isBinary) => {
  if (isBinary) frameCount++;
  else gotMeta = true;
});
ws.on('error', e => {
  console.error('[verify] ws error:', e.message);
  process.exit(1);
});

ws.on('open', async () => {
  console.log('[verify] ws connected to relay');

  // ① 正向:等首帧到达
  let waited = 0;
  while (frameCount === 0 && waited < 5000) {
    await sleep(200);
    waited += 200;
  }
  console.log(`[verify] forward: ${frameCount} frames, meta=${gotMeta} (waited ${waited}ms)`);

  // 连同一个 Chrome page(只读校验 DOM)
  const browser = await puppeteer.connect({ browserURL: 'http://127.0.0.1:9222', defaultViewport: null });
  const [page] = await browser.pages();

  // ② 反向点击计数器按钮
  const btn = await page.evaluate(() => {
    const b = document.querySelector('button:not(.x)');
    const r = b.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  });
  const c0 = await page.evaluate(() => document.getElementById('count').textContent);
  ws.send(JSON.stringify({ t: 'mp', x: btn.x, y: btn.y, btn: 'left', count: 1 }));
  await sleep(120);
  ws.send(JSON.stringify({ t: 'mr', x: btn.x, y: btn.y, btn: 'left', count: 1 }));
  await sleep(500);
  const c1 = await page.evaluate(() => document.getElementById('count').textContent);
  console.log(`[verify] counter via ws: ${c0} -> ${c1}  (btn@${btn.x.toFixed(0)},${btn.y.toFixed(0)})`);

  // ③ 反向关闭弹框 ✕
  const xx = await page.evaluate(() => {
    const b = document.querySelector('.modal .x');
    const r = b.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  });
  const m0 = await page.evaluate(() => !document.getElementById('modal').classList.contains('hidden'));
  ws.send(JSON.stringify({ t: 'mp', x: xx.x, y: xx.y, btn: 'left', count: 1 }));
  await sleep(120);
  ws.send(JSON.stringify({ t: 'mr', x: xx.x, y: xx.y, btn: 'left', count: 1 }));
  await sleep(500);
  const m1 = await page.evaluate(() => !document.getElementById('modal').classList.contains('hidden'));
  console.log(`[verify] modal via ws: visible ${m0} -> ${m1}  (x@${xx.x.toFixed(0)},${xx.y.toFixed(0)})`);

  // ④ 反向文字输入
  const ib = await page.evaluate(() => {
    const i = document.getElementById('txt');
    i.focus();
    const r = i.getBoundingClientRect();
    return { x: r.x + 20, y: r.y + r.height / 2 };
  });
  ws.send(JSON.stringify({ t: 'mp', x: ib.x, y: ib.y, btn: 'left', count: 1 }));
  await sleep(120);
  ws.send(JSON.stringify({ t: 'mr', x: ib.x, y: ib.y, btn: 'left', count: 1 }));
  await sleep(200);
  for (const ch of 'hello') {
    ws.send(JSON.stringify({ t: 'ti', text: ch }));
    await sleep(50);
  }
  await sleep(400);
  const v = await page.evaluate(() => document.getElementById('txt').value);
  console.log(`[verify] text via ws: input.value="${v}"`);

  const pass = frameCount > 0 && gotMeta && c1 !== c0 && m1 === false && v === 'hello';

  console.log('\n========== RESULT ==========');
  console.log(`forward frames : ${frameCount > 0 ? 'OK' : 'FAIL'}   (${frameCount} frames received)`);
  console.log(`forward meta   : ${gotMeta ? 'OK' : 'FAIL'}`);
  console.log(`reverse click  : ${c1 !== c0 ? 'OK' : 'FAIL'}   (count ${c0} -> ${c1})`);
  console.log(`reverse modal  : ${m1 === false ? 'OK' : 'FAIL'}   (visible=${m1})`);
  console.log(`reverse text   : ${v === 'hello' ? 'OK' : 'FAIL'}   (value="${v}")`);
  console.log(`OVERALL        : ${pass ? 'PASS ✅' : 'CHECK ⚠️'}`);

  process.exit(pass ? 0 : 2);
});
