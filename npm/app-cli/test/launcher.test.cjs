const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const os = require('node:os');
const { execFileSync } = require('node:child_process');
const { EventEmitter } = require('node:events');
const source = fs.readFileSync(path.join(__dirname, '../bin/cli.js'), 'utf8');

for (const [platform, arch, suffix] of [['darwin', 'arm64', 'darwin-arm64'], ['darwin', 'x64', 'darwin-x64'], ['linux', 'x64', 'linux-x64'], ['win32', 'x64', 'windows-x64']]) {
  test(`${suffix}: launcher selects paired Pingap and respects explicit env`, () => {
    for (const explicit of [undefined, '/custom/pingap']) {
      let captured;
      const env = explicit ? { APP_CLI_PINGAP_BIN: explicit } : {};
      const req = name => ({ child_process: { spawn: (...args) => { captured = args; return new EventEmitter(); } }, fs: { existsSync: () => true }, path })[name];
      req.resolve = () => `/packages/${suffix}/package.json`;
      vm.runInNewContext(source, { require: req, console, process: { platform, arch, env, argv: ['node', 'cli', 'serve'], on() {}, exit(code) { throw new Error(`unexpected exit ${code}`); } } });
      const selectedEnv = captured[2].env || env;
      assert.equal(selectedEnv.APP_CLI_PINGAP_BIN, explicit || `/packages/${suffix}/${platform === 'win32' ? 'pingap.exe' : 'pingap'}`);
      assert.equal(captured[1][0], 'serve');
    }
  });
  test(`${suffix}: npm tarball includes both executables`, () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'app-cli-pack-'));
    try {
      fs.copyFileSync(path.join(__dirname, `../../app-cli-${suffix}/package.json`), path.join(root, 'package.json'));
      const binaries = platform === 'win32' ? ['app-cli.exe', 'pingap.exe'] : ['app-cli', 'pingap'];
      for (const name of binaries) fs.writeFileSync(path.join(root, name), 'packaging fixture');
      const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
      const result = JSON.parse(execFileSync(npm, ['pack', '--dry-run', '--ignore-scripts', '--json'], { cwd: root, encoding: 'utf8', shell: process.platform === 'win32' }));
      const files = result[0].files.map(file => file.path);
      for (const name of binaries) assert.ok(files.includes(name), `missing ${name}`);
    } finally { fs.rmSync(root, { recursive: true, force: true }); }
  });
}
