#!/usr/bin/env node
'use strict';

// Component regression: actual built-in HTML/JavaScript and locale resources
// with a minimal DOM. This does not establish real-browser/container coverage.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const root = path.resolve(__dirname, '../../../../..');
const pagePath = 'crates/rcoder-proxy/assets/userapp-error-default.html';
const template = fs.readFileSync(path.join(root, pagePath), 'utf8');
const uiKeys = {
  BROWSER: 'browser', CONNECTED: 'connected', GATEWAY: 'gateway', NORMAL: 'normal',
  APP: 'app_service', UNAVAILABLE: 'unavailable', STARTING: 'starting',
  STOPPED: 'stopped', FAILED: 'failed', RELOAD: 'reload', HINT: 'hint',
  COPY: 'copy', COPIED: 'copied', ARIA: 'aria_chain',
};
const causes = [
  'starting', 'stopped', 'failed', 'missing', 'blocked', 'recovery_required',
  'platform_unavailable', 'outcome_unknown', 'generic',
];

function escapeHtml(text) {
  return text.replace(/[&<>"']/g, value => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#x27;',
  }[value]));
}

function decodeHtml(text) {
  return text.replace(/&(amp|lt|gt|quot|#x27);/g, (_, value) => ({
    amp: '&', lt: '<', gt: '>', quot: '"', '#x27': "'",
  }[value]));
}

function readLocale(locale) {
  const text = fs.readFileSync(path.join(root, `crates/shared_types_i18n/locales/${locale}.yml`), 'utf8');
  const strings = new Map();
  for (const line of text.split('\n')) {
    const match = line.match(/^(userapp_error_page\.[^:]+): (".*")$/);
    if (match) { strings.set(match[1], JSON.parse(match[2])); }
  }
  return key => {
    assert.ok(strings.has(key), `Missing actual locale key ${locale}: ${key}`);
    return strings.get(key);
  };
}

function element(openingTag) {
  assert.ok(openingTag, 'Expected actual HTML element is missing');
  const attrs = new Map();
  for (const match of openingTag.matchAll(/([\w-]+)="([^"]*)"/g)) {
    attrs.set(match[1], decodeHtml(match[2]));
  }
  return { className: attrs.get('class'), getAttribute: name => attrs.get(name) ?? null };
}

async function main() {
  let checked = 0;
  for (const locale of ['zh-CN', 'zh-TW', 'en-US']) {
    const t = readLocale(locale);
    for (const cause of causes) {
      const values = {
        RCODER_LANG: locale, RCODER_CAUSE: cause, RCODER_STATUS: '503',
        RCODER_TITLE: t(`userapp_error_page.${cause}.title`),
        RCODER_MESSAGE: t(`userapp_error_page.${cause}.message`),
        RCODER_DIAGNOSTIC_ID: 'fixture-diagnostic-id',
      };
      for (const [variable, key] of Object.entries(uiKeys)) {
        values[`RCODER_UI_${variable}`] = t(`userapp_error_page.ui.${key}`);
      }
      const html = template.replace(/\{\{(RCODER_[A-Z_]+)\}\}/g, (_, key) => {
        assert.ok(Object.hasOwn(values, key), `Unsupported actual placeholder ${key}`);
        return escapeHtml(values[key]);
      });
      assert.ok(!html.includes('{{RCODER_'), 'All actual placeholders must be resolved');
      assert.ok(html.includes(`>${escapeHtml(t('userapp_error_page.ui.reload'))}</button>`));
      const script = html.match(/<script>([\s\S]*?)<\/script>/);
      assert.ok(script, 'Actual embedded script is missing');
      const card = element(html.match(/<main\b[^>]*class="card"[^>]*>/)?.[0]);
      const nodes = {};
      for (const id of ['node-app', 'node-app-glyph', 'node-app-state', 'link-app']) {
        const opening = html.match(new RegExp(`<[^>]*\\bid="${id}"[^>]*>`));
        nodes[id] = element(opening?.[0]);
      }
      const glyph = html.match(/<span\b[^>]*id="node-app-glyph"[^>]*>([^<]*)<\/span>/);
      assert.ok(glyph, 'Actual app glyph span is missing');
      nodes['node-app-glyph'].innerHTML = glyph[1];
      const state = html.match(/<span\b[^>]*id="node-app-state"[^>]*>([^<]*)<\/span>/);
      assert.ok(state, 'Actual app state span is missing');
      nodes['node-app-state'].textContent = decodeHtml(state[1]);
      const timers = [];
      const copied = [];
      const context = vm.createContext({
        document: {
          querySelector: selector => {
            assert.equal(selector, 'main.card', 'Classification must not inspect the title');
            return card;
          },
          getElementById: id => {
            assert.ok(Object.hasOwn(nodes, id), `Unexpected actual DOM access ${id}`);
            return nodes[id];
          },
        },
        navigator: { clipboard: { writeText: async text => { copied.push(text); } } },
        setTimeout: callback => { timers.push(callback); },
      });
      vm.runInContext(script[1], context, { timeout: 1000, filename: pagePath });
      const expected = cause === 'starting' ? 'warn' : cause === 'stopped' ? 'idle' : 'bad';
      assert.equal(nodes['node-app'].className, `node ${expected}`, `${locale}/${cause}: style`);
      const stateKey = ['starting', 'stopped', 'failed'].includes(cause) ? cause : 'unavailable';
      assert.equal(nodes['node-app-state'].textContent, t(`userapp_error_page.ui.${stateKey}`));
      if (cause === 'starting') {
        assert.equal(nodes['node-app-glyph'].innerHTML, '<span class="spin"></span>');
        assert.equal(nodes['link-app'].className, 'link warn');
      } else if (cause === 'stopped') {
        assert.equal(nodes['node-app-glyph'].innerHTML, '<span class="bar"></span>');
        assert.equal(nodes['link-app'].className, 'link');
      } else {
        assert.equal(nodes['node-app-glyph'].innerHTML, '✕');
        assert.equal(nodes['link-app'].className, 'link bad');
      }
      if (cause === 'generic') {
        const chip = element(html.match(/<span\b[^>]*class="diag"[^>]*>/)?.[0]);
        const labelSpan = html.match(/<span class="copy">([^<]*)<\/span>/);
        const idSpan = html.match(/<span class="id">([^<]*)<\/span>/);
        assert.ok(labelSpan && idSpan, 'Actual clipboard label/ID spans are missing');
        const label = { textContent: decodeHtml(labelSpan[1]) };
        assert.equal(label.textContent, t('userapp_error_page.ui.copy'));
        chip.querySelector = selector => {
          assert.ok(selector === '.id' || selector === '.copy');
          return selector === '.id' ? { textContent: decodeHtml(idSpan[1]) } : label;
        };
        context.copyDiag(chip);
        await Promise.resolve();
        assert.deepEqual(copied, [values.RCODER_DIAGNOSTIC_ID]);
        assert.equal(label.textContent, t('userapp_error_page.ui.copied'));
        assert.equal(timers.length, 1);
        timers[0]();
        assert.equal(label.textContent, t('userapp_error_page.ui.copy'));
      }
      checked += 1;
    }
  }
  process.stdout.write(`PASS: ${checked} actual-script locale/cause cases plus 3 clipboard cases; component DOM fixture only.\n`);
}

main().catch(error => {
  process.stderr.write(`${error.stack}\n`);
  process.exitCode = 1;
});
