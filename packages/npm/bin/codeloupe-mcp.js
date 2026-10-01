#!/usr/bin/env node
'use strict';

const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
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

// Windows locks a running executable, so npm cannot replace a package whose
// binary is in use. Packaged binaries therefore run from a versioned copy.
const RUNTIME_DIRECTORY_PATTERN = /^[0-9A-Za-z.+_-]+-[0-9a-f]{16}$/;
const RUNTIME_STALE_MS = 24 * 60 * 60 * 1000;
const RUNTIME_TEMPORARY_STALE_MS = 60 * 60 * 1000;
const RENAME_ATTEMPTS = 5;
const RENAME_RETRY_MS = 50;

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
    items.push({ path: override, packaged: false });
  }
  items.push({ path: platformPackageBinary(name), packaged: true });
  items.push({ path: path.join(packageRoot, 'native', platformKey(), name), packaged: true });
  if (fs.existsSync(path.join(repoRoot, 'Cargo.toml'))) {
    items.push({ path: path.join(repoRoot, 'target', 'release', name), packaged: false });
  }
  return items;
}

function findBinary() {
  for (const candidate of candidates()) {
    if (candidate.path && fs.existsSync(candidate.path)) {
      return candidate;
    }
  }
  return null;
}

function runtimeCopyEnabled(environment = process.env, platform = process.platform) {
  if (platform !== 'win32') {
    return false;
  }
  const value = String(environment.CODELOUPE_MCP_RUNTIME_COPY || '').trim().toLowerCase();
  return !['0', 'false', 'no', 'off'].includes(value);
}

function runtimeRoot(environment = process.env) {
  if (environment.CODELOUPE_MCP_RUNTIME_DIR) {
    return path.resolve(environment.CODELOUPE_MCP_RUNTIME_DIR);
  }
  const localAppData = environment.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
  return path.join(localAppData, 'codeloupe-mcp', 'runtime');
}

function launcherVersion() {
  try {
    return require(path.join(packageRoot, 'package.json')).version || '0.0.0';
  } catch (_error) {
    return '0.0.0';
  }
}

function sha256File(file) {
  const hash = crypto.createHash('sha256');
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  const descriptor = fs.openSync(file, 'r');
  try {
    let bytesRead;
    while ((bytesRead = fs.readSync(descriptor, buffer, 0, buffer.length, null)) > 0) {
      hash.update(buffer.subarray(0, bytesRead));
    }
  } finally {
    fs.closeSync(descriptor);
  }
  return hash.digest('hex');
}

function isCompleteCopy(file, size, digest) {
  try {
    return fs.statSync(file).size === size && sha256File(file) === digest;
  } catch (_error) {
    return false;
  }
}

function sleepSync(milliseconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, milliseconds);
}

// Copies `source` into `<root>/<version>-<sha256 prefix>/` and returns the copy.
// Concurrent launchers write distinct temporary files and rename them into place;
// a copy that another launcher already placed (and may be running) is reused.
function stageRuntimeBinary(source, options = {}) {
  const root = options.root || runtimeRoot();
  const version = options.version || launcherVersion();
  const now = options.now || Date.now();
  const size = fs.statSync(source).size;
  const digest = sha256File(source);
  const directory = path.join(root, `${version}-${digest.slice(0, 16)}`);
  const target = path.join(directory, path.basename(source));
  fs.mkdirSync(directory, { recursive: true });

  if (!isCompleteCopy(target, size, digest)) {
    const temporary = path.join(directory, `.${path.basename(source)}.${process.pid}.${now}.tmp`);
    fs.copyFileSync(source, temporary);
    try {
      for (let attempt = 1; ; attempt += 1) {
        try {
          fs.renameSync(temporary, target);
          break;
        } catch (error) {
          if (isCompleteCopy(target, size, digest) || attempt >= RENAME_ATTEMPTS) {
            throw error;
          }
          sleepSync(RENAME_RETRY_MS);
        }
      }
    } catch (error) {
      fs.rmSync(temporary, { force: true });
      if (!isCompleteCopy(target, size, digest)) {
        throw error;
      }
    }
  }

  const touchedAt = new Date(now);
  fs.utimesSync(directory, touchedAt, touchedAt);
  return target;
}

function isRuntimeEntry(name) {
  return name === 'codeloupe-mcp.exe'
    || name === 'codeloupe-mcp'
    || /^\.codeloupe-mcp(\.exe)?\.\d+\.\d+\.tmp$/.test(name);
}

// Removes copies unused for a day and stale temporary files. Only directories
// that look like runtime copies and contain nothing else are touched; a copy
// that is still running stays locked and is retried on a later start.
function pruneRuntimeCopies(root, keepDirectory, options = {}) {
  const now = options.now || Date.now();
  const maxAgeMs = options.maxAgeMs || RUNTIME_STALE_MS;
  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch (_error) {
    return;
  }
  const keep = keepDirectory ? path.resolve(keepDirectory) : null;
  for (const entry of entries) {
    if (!entry.isDirectory() || !RUNTIME_DIRECTORY_PATTERN.test(entry.name)) {
      continue;
    }
    const directory = path.join(root, entry.name);
    try {
      const names = fs.readdirSync(directory);
      if (!names.every(isRuntimeEntry)) {
        continue;
      }
      if (path.resolve(directory) === keep) {
        for (const name of names.filter((item) => item.endsWith('.tmp'))) {
          const file = path.join(directory, name);
          if (now - fs.statSync(file).mtimeMs > RUNTIME_TEMPORARY_STALE_MS) {
            fs.rmSync(file, { force: true });
          }
        }
        continue;
      }
      if (now - fs.statSync(directory).mtimeMs > maxAgeMs) {
        fs.rmSync(directory, { recursive: true, force: true });
      }
    } catch (_error) {
      // Locked or concurrently removed; leave it for a later start.
    }
  }
}

function executableForLaunch(candidate, environment = process.env) {
  if (!candidate.packaged || !runtimeCopyEnabled(environment)) {
    return candidate.path;
  }
  try {
    const root = runtimeRoot(environment);
    const executable = stageRuntimeBinary(candidate.path, { root });
    pruneRuntimeCopies(root, path.dirname(executable));
    return executable;
  } catch (error) {
    console.error(`codeloupe-mcp: could not stage a runtime copy (${error.message}); running the installed binary directly.`);
    return candidate.path;
  }
}

function main() {
  let candidate;
  try {
    candidate = findBinary();
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }

  if (!candidate) {
    console.error([
      'codeloupe-mcp binary was not found for this platform.',
      `Expected optional package: ${platformPackages[platformKey()]}`,
      'Reinstall with optional dependencies enabled, install from GitHub Releases, run cargo build --release, or set CODELOUPE_MCP_BINARY.',
    ].join('\n'));
    process.exit(1);
  }

  const child = spawn(executableForLaunch(candidate), process.argv.slice(2), {
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

module.exports = {
  binaryOverride,
  platformKey,
  platformPackageBinary,
  pruneRuntimeCopies,
  runtimeCopyEnabled,
  runtimeRoot,
  stageRuntimeBinary,
};
