#!/usr/bin/env node
'use strict';

// Read-only reproduction using the repository's actual page JavaScript and
// locale resources. Exit 0 means the reviewed defect is reproduced; this is
// neither a passing product regression test nor a browser/container E2E test.
// Usage: node proxy_page_dom.cjs [--output /explicit/new/result.json]
// --output exclusively creates a new file and never overwrites an existing one.

const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const path = require('node:path');

function main() {
  const args = process.argv.slice(2);
  if (args.length !== 0 && (args.length !== 2 || args[0] !== '--output' || !args[1])) {
    throw new Error('Usage: node proxy_page_dom.cjs [--output /explicit/new/result.json]');
  }

  // fixtures -> dated review -> reviews -> development -> docs -> repository.
  const root = path.resolve(__dirname, '../../../../..');
  const pagePath = 'crates/rcoder-proxy/assets/userapp-error-default.html';
  const html = fs.readFileSync(path.join(root, pagePath), 'utf8');
  const scriptMatch = html.match(/<script>([\s\S]*?)<\/script>/);
  assert.ok(scriptMatch, 'Actual built-in page must contain its embedded script');

  const observations = [];
  for (const locale of ['zh-CN', 'zh-TW', 'en-US']) {
    const localePath = `crates/shared_types_i18n/locales/${locale}.yml`;
    const resource = fs.readFileSync(path.join(root, localePath), 'utf8');
    for (const cause of ['starting', 'stopped', 'failed']) {
      const key = `userapp_error_page.${cause}.title`;
      const titleMatch = resource.match(new RegExp(`^userapp_error_page\\.${cause}\\.title: (".*")$`, 'm'));
      assert.ok(titleMatch, `Actual locale resource must contain ${key} in ${locale}`);
      const title = JSON.parse(titleMatch[1]);

      // Minimal DOM fixture for the four elements the actual page script uses.
      // The baseline HTML state is checked explicitly to avoid silently mocking
      // a state that no longer matches the source under review.
      assert.match(html, /class="node bad" id="node-app"/);
      assert.match(html, /id="node-app-glyph"[^>]*>✕<\/span>/);
      assert.match(html, /id="node-app-state">不可用<\/span>/);
      assert.match(html, /class="link bad" id="link-app"/);
      const nodes = {
        'node-app': { className: 'node bad' },
        'node-app-glyph': { innerHTML: '✕' },
        'node-app-state': { textContent: '不可用' },
        'link-app': { className: 'link bad' },
      };
      vm.runInNewContext(scriptMatch[1], {
        document: {
          querySelector: (selector) => {
            assert.equal(selector, 'h1');
            return { textContent: title };
          },
          getElementById: (id) => {
            assert.ok(Object.hasOwn(nodes, id), `Unexpected DOM access: ${id}`);
            return nodes[id];
          },
        },
      }, { timeout: 1000, filename: pagePath });
      observations.push({
        locale, cause, title,
        className: nodes['node-app'].className,
        displayedState: nodes['node-app-state'].textContent,
        glyph: nodes['node-app-glyph'].innerHTML,
      });
    }
  }

  const find = (locale, cause) => observations.find(row => row.locale === locale && row.cause === cause);
  assert.equal(find('zh-CN', 'starting').className, 'node warn', 'Simplified Chinese control case must show starting');
  assert.equal(find('en-US', 'starting').className, 'node bad', 'Reproduce English starting incorrectly keeping red failure styling');
  assert.equal(find('en-US', 'stopped').displayedState, '不可用', 'Reproduce English stopped incorrectly keeping unavailable text');
  assert.equal(find('zh-TW', 'starting').displayedState, '不可用', 'Reproduce Traditional Chinese starting incorrectly keeping unavailable text');

  const result = JSON.stringify({
    meaning_of_exit_zero: 'The reviewed defect is reproduced; not a passing product regression test.',
    scope: 'Actual embedded page script + actual locale titles with a minimal DOM fixture; not full browser/container E2E.',
    page_source: pagePath,
    observations,
  }, null, 2) + '\n';
  if (args.length === 2) {
    fs.writeFileSync(path.resolve(args[1]), result, { flag: 'wx' });
  }
  process.stdout.write(result);
}

try {
  main();
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = 1;
}
