#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

const packageRoot = path.resolve(__dirname, '..');
const repoRoot = path.resolve(packageRoot, '..', '..');
const platformPackages = {
  'darwin-arm64': '@ndhkaeru/codeloupe-mcp-darwin-arm64',
  'darwin-x64': '@ndhkaeru/codeloupe-mcp-darwin-x64',
  'linux-arm64': '@ndhkaeru/codeloupe-mcp-linux-arm64',
  'linux-x64': '@ndhkaeru/codeloupe-mcp-linux-x64',
  'win32-arm64': '@ndhkaeru/codeloupe-mcp-win32-arm64',
  'win32-x64': '@ndhkaeru/codeloupe-mcp-win32-x64',
};

function platformKey() {
  const platform = process.platform;
  const arch = process.arch;
  const supported = new Set([
    'darwin-arm64',
    'darwin-x64',
    'linux-arm64',
    'linux-x64',
    'win32-arm64',
    'win32-x64',
  ]);
  const key = `${platform}-${arch}`;
  if (!supported.has(key)) {
    throw new Error(`Unsupported platform: ${key}`);
  }
  return key;
}

function executableName() {
  return process.platform === 'win32' ? 'codeloupe-mcp.exe' : 'codeloupe-mcp';
}

function binaryOverride(environment = process.env) {
  return environment.CODELOUPE_MCP_BINARY
    || environment.codeloupe_mcp_BINARY
    || environment.CODEBASE_MCP_BINARY
    || null;
}

function platformPackageBinary(name, resolvePackage = require.resolve) {
  const packageName = platformPackages[platformKey()];
  try {
    return resolvePackage(`${packageName}/bin/${name}`);
  } catch (_error) {
    return null;
  }
}

function candidates(environment = process.env) {
  const name = executableName();
  const items = [];
  const override = binaryOverride(environment);
  if (override) {
    items.push(override);
  }
  items.push(platformPackageBinary(name));
  items.push(path.join(packageRoot, 'native', platformKey(), name));
  if (fs.existsSync(path.join(repoRoot, 'Cargo.toml'))) {
    items.push(path.join(repoRoot, 'target', 'release', name));
  }
  return items;
}

function findBinary() {
  for (const candidate of candidates()) {
    if (candidate && fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return null;
}

function main() {
  let binary;
  try {
    binary = findBinary();
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }

  if (!binary) {
    console.error([
      'codeloupe-mcp binary was not found for this platform.',
      `Expected optional package: ${platformPackages[platformKey()]}`,
      'Reinstall with optional dependencies enabled, install from GitHub Releases, run cargo build --release, or set CODELOUPE_MCP_BINARY.',
    ].join('\n'));
    process.exit(1);
  }

  const child = spawn(binary, process.argv.slice(2), {
    stdio: 'inherit',
    windowsHide: true,
  });

  child.on('error', (error) => {
    console.error(error.message);
    process.exit(1);
  });

  child.on('exit', (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 0);
  });
}

if (require.main === module) {
  main();
}

module.exports = { binaryOverride, platformKey, platformPackageBinary };
