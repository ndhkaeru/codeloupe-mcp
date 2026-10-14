'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { test } = require('node:test');
const { syncVersion } = require('./sync-version.js');

const repositoryRoot = path.resolve(__dirname, '..', '..', '..');
const platforms = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64'];
const files = ['Cargo.toml', 'Cargo.lock', 'packages/npm/package.json', 'server.json',
  ...platforms.map((platform) => 'packages/npm-platforms/' + platform + '/package.json')];
const baseline = '1.1.3';
const next = '2.0.0-rc.1+build.42';

function fixture(run, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'codeloupe-version-sync-'));
  try {
    for (const relative of files) {
      let content = fs.readFileSync(path.join(repositoryRoot, relative), 'utf8');
      if (relative === 'Cargo.toml') {
        content = content.replace(/^version = "[^"]*"$/m, 'version = "' + baseline + '"');
      } else if (relative === 'Cargo.lock') {
        content = content.replace(/(name = "codeloupe-mcp"\r?\nversion = ")[^"]*"/,
          '$1' + baseline + '"');
        content += '\n[[package]]\nname = "version-sync-fixture"\nversion = "' + baseline + '"\n';
      } else {
        const manifest = JSON.parse(content);
        manifest.version = baseline;
        if (relative === 'server.json') manifest.packages[0].version = baseline;
        if (relative === 'packages/npm/package.json') {
          for (const name of Object.keys(manifest.optionalDependencies)) manifest.optionalDependencies[name] = baseline;
          manifest.description += ' Literal example: "version": "' + baseline + '"';
        }
        content = JSON.stringify(manifest, null, 2) + '\n';
      }
      if (options.crlf) content = content.replace(/\r?\n/g, '\r\n');
      const filename = path.join(root, relative);
      fs.mkdirSync(path.dirname(filename), { recursive: true });
      fs.writeFileSync(filename, content);
    }
    run(root);
  } finally {
    assert.equal(path.dirname(root), path.resolve(os.tmpdir()));
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function snapshot(root) {
  return Object.fromEntries(files.map((relative) => [
    relative, fs.existsSync(path.join(root, relative)) ? fs.readFileSync(path.join(root, relative), 'utf8') : null,
  ]));
}

function setVersion(root, version = next) {
  const filename = path.join(root, 'Cargo.toml');
  fs.writeFileSync(filename, fs.readFileSync(filename, 'utf8').replace(
    /^version = "[^"]*"(\r?)$/m, 'version = "' + version + '"$1'));
}

function json(root, relative) {
  return JSON.parse(fs.readFileSync(path.join(root, relative), 'utf8'));
}

test('check rejects drift without changing any file', () => fixture((root) => {
  setVersion(root);
  const before = snapshot(root);
  assert.throws(() => syncVersion(root, { check: true }), /out of sync/);
  assert.deepEqual(snapshot(root), before);
}));

test('sync updates all version fields, preserves descriptions and unrelated lock packages', () => fixture((root) => {
  setVersion(root);
  const before = snapshot(root);
  const result = syncVersion(root);
  assert.equal(result.version, next);
  assert.equal(result.changed.length, 9);
  const main = json(root, 'packages/npm/package.json');
  assert.equal(main.version, next);
  assert.equal(main.description, JSON.parse(before['packages/npm/package.json']).description);
  assert.deepEqual(Object.values(main.optionalDependencies), platforms.map(() => next));
  for (const platform of platforms) {
    assert.equal(json(root, 'packages/npm-platforms/' + platform + '/package.json').version, next);
  }
  const server = json(root, 'server.json');
  assert.equal(server.version, next);
  assert.equal(server.packages[0].version, next);
  const lock = snapshot(root)['Cargo.lock'];
  assert(lock.includes('name = "codeloupe-mcp"\nversion = "' + next + '"'));
  assert(lock.includes('name = "version-sync-fixture"\nversion = "' + baseline + '"'));
  const synchronized = snapshot(root);
  assert.deepEqual(syncVersion(root).changed, []);
  syncVersion(root, { check: true });
  assert.deepEqual(snapshot(root), synchronized);
}));

test('sync preserves CRLF and compact JSON formatting', () => fixture((root) => {
  const filename = path.join(root, 'packages/npm-platforms/darwin-arm64/package.json');
  fs.writeFileSync(filename, JSON.stringify(json(root, 'packages/npm-platforms/darwin-arm64/package.json')) + '\r\n');
  setVersion(root);
  syncVersion(root);
  for (const [relative, content] of Object.entries(snapshot(root))) {
    assert(!/(?<!\r)\n/.test(content), relative + ' lost CRLF');
  }
  const compact = fs.readFileSync(filename, 'utf8');
  assert.equal(compact.split('\r\n').length, 2);
  assert.equal(JSON.parse(compact).version, next);
}, { crlf: true }));

test('sync accepts a single-quoted Cargo version and comments without rewriting Cargo.toml', () => fixture((root) => {
  const filename = path.join(root, 'Cargo.toml');
  fs.writeFileSync(filename, fs.readFileSync(filename, 'utf8').replace(
    'version = "' + baseline + '"', "version = '" + next + "' # next release"));
  const cargo = fs.readFileSync(filename, 'utf8');
  syncVersion(root);
  assert.equal(fs.readFileSync(filename, 'utf8'), cargo);
  assert.equal(json(root, 'packages/npm/package.json').version, next);
}));

test('invalid semantic versions fail before mutation', () => {
  for (const version of ['01.2.3', '1.2', 'v1.2.3', '1.2.3-01', '1.2.3+']) {
    fixture((root) => {
      setVersion(root, version);
      const before = snapshot(root);
      assert.throws(() => syncVersion(root), /invalid semantic version/);
      assert.deepEqual(snapshot(root), before);
    });
  }
});

test('malformed final metadata and a missing platform file are preflighted before writes', () => {
  for (const defect of ['json', 'missing']) {
    fixture((root) => {
      setVersion(root);
      if (defect === 'json') fs.writeFileSync(path.join(root, 'server.json'), '{invalid');
      else fs.rmSync(path.join(root, 'packages/npm-platforms/win32-x64/package.json'));
      const before = snapshot(root);
      assert.throws(() => syncVersion(root));
      assert.deepEqual(snapshot(root), before);
    });
  }
});

test('duplicate root lock entries and mismatched registry identity are rejected', () => {
  for (const defect of ['duplicate', 'identity', 'missing_identity', 'dependency', 'compatibility']) {
    fixture((root) => {
      setVersion(root);
      if (defect === 'duplicate') {
        fs.appendFileSync(path.join(root, 'Cargo.lock'),
          '\n[[package]]\nname = "codeloupe-mcp"\nversion = "' + baseline + '"\n');
      } else if (defect === 'identity') {
        const server = json(root, 'server.json');
        server.name = 'unexpected';
        fs.writeFileSync(path.join(root, 'server.json'), JSON.stringify(server));
      } else if (defect === 'missing_identity') {
        const main = json(root, 'packages/npm/package.json');
        const server = json(root, 'server.json');
        delete main.mcpName;
        delete server.name;
        fs.writeFileSync(path.join(root, 'packages/npm/package.json'), JSON.stringify(main));
        fs.writeFileSync(path.join(root, 'server.json'), JSON.stringify(server));
      } else if (defect === 'dependency') {
        const main = json(root, 'packages/npm/package.json');
        delete main.optionalDependencies['@ndhkaeru/codeloupe-mcp-win32-x64'];
        fs.writeFileSync(path.join(root, 'packages/npm/package.json'), JSON.stringify(main));
      } else {
        const file = 'packages/npm-platforms/win32-x64/package.json';
        const manifest = json(root, file);
        manifest.cpu = ['arm64'];
        fs.writeFileSync(path.join(root, file), JSON.stringify(manifest));
      }
      const before = snapshot(root);
      assert.throws(() => syncVersion(root));
      assert.deepEqual(snapshot(root), before);
    });
  }
});

test('a failed write restores every previously updated file', () => fixture((root) => {
  setVersion(root);
  const before = snapshot(root);
  let writes = 0;
  assert.throws(() => syncVersion(root, {
    writeFile(filename, content) {
      writes += 1;
      if (writes === 3) throw new Error('injected write failure');
      fs.writeFileSync(filename, content);
    },
  }), /injected write failure/);
  assert.deepEqual(snapshot(root), before);
  syncVersion(root);
  syncVersion(root, { check: true });
}));

test('rollback refuses to overwrite an independent newer edit', () => fixture((root) => {
  setVersion(root);
  let first;
  let writes = 0;
  assert.throws(() => syncVersion(root, {
    writeFile(filename, content) {
      writes += 1;
      if (writes === 2) {
        fs.writeFileSync(first, 'independent contents');
        throw new Error('injected write failure');
      }
      first = filename;
      fs.writeFileSync(filename, content);
    },
  }), /Rollback incomplete/);
  assert.equal(fs.readFileSync(first, 'utf8'), 'independent contents');
}));

test('CLI derives the root from its script path and check/invalid arguments do not write', () => fixture((root) => {
  const script = path.join(root, 'packages/npm/scripts/sync-version.js');
  fs.mkdirSync(path.dirname(script), { recursive: true });
  fs.copyFileSync(path.join(__dirname, 'sync-version.js'), script);
  const execute = (args) => spawnSync(process.execPath, [script, ...args], { cwd: os.tmpdir(), encoding: 'utf8' });
  assert.equal(execute(['--check']).status, 0);
  const before = snapshot(root);
  assert.equal(execute(['--invalid']).status, 1);
  assert.deepEqual(snapshot(root), before);
  setVersion(root);
  const drifted = snapshot(root);
  const check = execute(['--check']);
  assert.equal(check.status, 1);
  assert.match(check.stderr, /out of sync/);
  assert.deepEqual(snapshot(root), drifted);
  assert.equal(execute([]).status, 0);
  assert.equal(execute(['--check']).status, 0);
}));
