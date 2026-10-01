'use strict';

const assert = require('assert');
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const launcher = require('../bin/codeloupe-mcp.js');

const DAY_MS = 24 * 60 * 60 * 1000;

function digestPrefix(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex').slice(0, 16);
}

function setAge(file, now, ageMs) {
  const time = new Date(now - ageMs);
  fs.utimesSync(file, time, time);
}

assert.strictEqual(launcher.runtimeCopyEnabled({}, 'win32'), true);
for (const value of ['0', 'false', 'OFF', ' no ']) {
  assert.strictEqual(launcher.runtimeCopyEnabled({ CODELOUPE_MCP_RUNTIME_COPY: value }, 'win32'), false, value);
}
assert.strictEqual(launcher.runtimeCopyEnabled({ CODELOUPE_MCP_RUNTIME_COPY: '1' }, 'win32'), true);
assert.strictEqual(launcher.runtimeCopyEnabled({}, 'linux'), false);
assert.strictEqual(launcher.runtimeCopyEnabled({}, 'darwin'), false);

assert.strictEqual(
  launcher.runtimeRoot({ LOCALAPPDATA: path.join('C:', 'Users', 'u', 'AppData', 'Local') }),
  path.join('C:', 'Users', 'u', 'AppData', 'Local', 'codeloupe-mcp', 'runtime')
);
assert.strictEqual(
  launcher.runtimeRoot({ CODELOUPE_MCP_RUNTIME_DIR: path.join(os.tmpdir(), 'custom-runtime'), LOCALAPPDATA: 'ignored' }),
  path.resolve(path.join(os.tmpdir(), 'custom-runtime'))
);

const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'codeloupe-mcp-runtime-test-'));
try {
  const installed = path.join(temporaryRoot, 'node_modules', 'pkg', 'bin');
  fs.mkdirSync(installed, { recursive: true });
  const source = path.join(installed, 'codeloupe-mcp.exe');
  fs.writeFileSync(source, crypto.randomBytes(4096));
  const root = path.join(temporaryRoot, 'runtime');
  const now = Date.now();

  // First launch copies into a version + content addressed directory.
  const first = launcher.stageRuntimeBinary(source, { root, version: '9.9.9', now });
  assert.strictEqual(first, path.join(root, `9.9.9-${digestPrefix(source)}`, 'codeloupe-mcp.exe'));
  assert.ok(fs.readFileSync(first).equals(fs.readFileSync(source)));

  // Later launches reuse the copy without rewriting it.
  const firstStat = fs.statSync(first, { bigint: true });
  const again = launcher.stageRuntimeBinary(source, { root, version: '9.9.9', now: now + 1000 });
  assert.strictEqual(again, first);
  assert.strictEqual(fs.statSync(again, { bigint: true }).ino, firstStat.ino, 'copy is not rewritten');

  const corrupted = fs.readFileSync(first);
  corrupted[0] ^= 0xff;
  fs.writeFileSync(first, corrupted);
  assert.strictEqual(fs.statSync(first).size, fs.statSync(source).size);
  launcher.stageRuntimeBinary(source, { root, version: '9.9.9', now });
  assert.ok(fs.readFileSync(first).equals(fs.readFileSync(source)), 'same-sized corrupted copy is replaced');

  // A different binary with the same version gets its own copy.
  fs.writeFileSync(source, crypto.randomBytes(4096));
  const second = launcher.stageRuntimeBinary(source, { root, version: '9.9.9', now });
  assert.notStrictEqual(path.dirname(second), path.dirname(first));
  assert.ok(fs.readFileSync(second).equals(fs.readFileSync(source)));

  // A copy placed by a concurrent launcher is reused as-is.
  const racedSource = path.join(installed, 'raced', 'codeloupe-mcp.exe');
  fs.mkdirSync(path.dirname(racedSource), { recursive: true });
  fs.writeFileSync(racedSource, crypto.randomBytes(2048));
  const racedTarget = path.join(root, `9.9.9-${digestPrefix(racedSource)}`, 'codeloupe-mcp.exe');
  fs.mkdirSync(path.dirname(racedTarget), { recursive: true });
  fs.copyFileSync(racedSource, racedTarget);
  const racedStat = fs.statSync(racedTarget, { bigint: true });
  assert.strictEqual(launcher.stageRuntimeBinary(racedSource, { root, version: '9.9.9', now }), racedTarget);
  assert.strictEqual(fs.statSync(racedTarget, { bigint: true }).ino, racedStat.ino, 'existing copy is not replaced');

  // Pruning removes only stale runtime-shaped directories that contain nothing else.
  const staleCopy = path.join(root, `1.0.0-${'a'.repeat(16)}`);
  fs.mkdirSync(staleCopy);
  fs.writeFileSync(path.join(staleCopy, 'codeloupe-mcp.exe'), 'old');
  setAge(staleCopy, now, 2 * DAY_MS);
  const recentCopy = path.join(root, `1.0.1-${'b'.repeat(16)}`);
  fs.mkdirSync(recentCopy);
  fs.writeFileSync(path.join(recentCopy, 'codeloupe-mcp.exe'), 'recent');
  setAge(recentCopy, now, 60 * 1000);
  const foreignName = path.join(root, 'keep-me');
  fs.mkdirSync(foreignName);
  setAge(foreignName, now, 30 * DAY_MS);
  const foreignContent = path.join(root, `1.0.2-${'c'.repeat(16)}`);
  fs.mkdirSync(foreignContent);
  fs.writeFileSync(path.join(foreignContent, 'codeloupe-mcp.exe'), 'x');
  fs.writeFileSync(path.join(foreignContent, 'notes.txt'), 'user file');
  setAge(foreignContent, now, 30 * DAY_MS);
  const keep = path.dirname(second);
  const staleTemporary = path.join(keep, '.codeloupe-mcp.exe.123.456.tmp');
  const freshTemporary = path.join(keep, '.codeloupe-mcp.exe.789.456.tmp');
  fs.writeFileSync(staleTemporary, 'partial');
  fs.writeFileSync(freshTemporary, 'partial');
  setAge(staleTemporary, now, 2 * 60 * 60 * 1000);
  setAge(keep, now, 3 * DAY_MS);

  launcher.pruneRuntimeCopies(root, keep, { now });
  assert.strictEqual(fs.existsSync(staleCopy), false, 'stale copy is removed');
  assert.strictEqual(fs.existsSync(recentCopy), true, 'recently used copy is kept');
  assert.strictEqual(fs.existsSync(foreignName), true, 'non-runtime directory name is ignored');
  assert.strictEqual(fs.existsSync(path.join(foreignContent, 'notes.txt')), true, 'directory with foreign files is ignored');
  assert.strictEqual(fs.existsSync(second), true, 'current copy is kept even when old');
  assert.strictEqual(fs.existsSync(staleTemporary), false, 'stale temporary file is removed');
  assert.strictEqual(fs.existsSync(freshTemporary), true, 'fresh temporary file of a concurrent launcher is kept');

  launcher.pruneRuntimeCopies(path.join(temporaryRoot, 'missing-root'), null, { now });
} finally {
  fs.rmSync(temporaryRoot, { recursive: true, force: true });
}

console.log('launcher runtime copy tests passed');
