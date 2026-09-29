'use strict';

const assert = require('assert');
const { binaryOverride } = require('../bin/codeloupe-mcp.js');

assert.strictEqual(
  binaryOverride({ CODELOUPE_MCP_BINARY: 'standard', codeloupe_mcp_BINARY: 'legacy' }),
  'standard',
);
assert.strictEqual(binaryOverride({ codeloupe_mcp_BINARY: 'legacy' }), 'legacy');
assert.strictEqual(binaryOverride({ CODEBASE_MCP_BINARY: 'old-name' }), 'old-name');
