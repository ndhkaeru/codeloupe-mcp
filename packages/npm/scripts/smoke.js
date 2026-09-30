'use strict';

const assert = require('assert');
const path = require('path');
const { spawnSync } = require('child_process');
const packageJson = require('../package.json');

const launcher = path.resolve(__dirname, '..', 'bin', 'codeloupe-mcp.js');
const result = spawnSync(process.execPath, [launcher, '--version'], {
  encoding: 'utf8',
  env: process.env,
  timeout: 10_000,
});

if (result.error) {
  throw result.error;
}
assert.strictEqual(result.status, 0, result.stderr || 'launcher exited with a non-zero status');
assert.strictEqual(result.stdout.trim(), `codeloupe-mcp ${packageJson.version}`);
assert.strictEqual(result.stderr.trim(), '');
