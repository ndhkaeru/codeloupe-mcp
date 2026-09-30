'use strict';

const assert = require('assert');
const fs = require('fs');
const path = require('path');

const packageRoot = path.resolve(__dirname, '..');
const platformRoot = path.resolve(packageRoot, '..', 'npm-platforms');
const mainPackage = require(path.join(packageRoot, 'package.json'));
const expected = {
  'darwin-arm64': { os: 'darwin', cpu: 'arm64' },
  'darwin-x64': { os: 'darwin', cpu: 'x64' },
  'linux-arm64': { os: 'linux', cpu: 'arm64' },
  'linux-x64': { os: 'linux', cpu: 'x64' },
  'win32-arm64': { os: 'win32', cpu: 'arm64' },
  'win32-x64': { os: 'win32', cpu: 'x64' },
};

assert(!mainPackage.files.includes('native/'), 'main npm package must not bundle every native binary');
assert.strictEqual(Object.keys(mainPackage.optionalDependencies || {}).length, 6);

for (const [platformKey, compatibility] of Object.entries(expected)) {
  const manifestPath = path.join(platformRoot, platformKey, 'package.json');
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  const expectedName = `@ndhkaeru/codeloupe-mcp-${platformKey}`;
  assert.strictEqual(manifest.name, expectedName);
  assert.strictEqual(manifest.version, mainPackage.version);
  assert.deepStrictEqual(manifest.os, [compatibility.os]);
  assert.deepStrictEqual(manifest.cpu, [compatibility.cpu]);
  assert.strictEqual(mainPackage.optionalDependencies[expectedName], mainPackage.version);
  assert(manifest.files.includes('bin/'));
}
