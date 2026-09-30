'use strict';

const assert = require('assert');
const launcher = require('../bin/codeloupe-mcp.js');

const platformKey = launcher.platformKey();
const packageName = `@ndhkaeru/codeloupe-mcp-${platformKey}`;
const executable = process.platform === 'win32' ? 'codeloupe-mcp.exe' : 'codeloupe-mcp';
const expectedRequest = `${packageName}/bin/${executable}`;
const resolvedBinary = `/resolved/${platformKey}/${executable}`;
let actualRequest = null;

assert.strictEqual(
  launcher.platformPackageBinary(executable, (request) => {
    actualRequest = request;
    return resolvedBinary;
  }),
  resolvedBinary
);
assert.strictEqual(actualRequest, expectedRequest);
assert.strictEqual(launcher.platformPackageBinary(executable, () => {
  throw new Error('missing package');
}), null);
