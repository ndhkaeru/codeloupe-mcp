'use strict';

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

const packageRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(packageRoot, '..', '..');
const mainPackage = require(path.join(packageRoot, 'package.json'));
const platformKey = `${process.platform}-${process.arch}`;
const platformPackageRoot = path.join(repositoryRoot, 'packages', 'npm-platforms', platformKey);

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    ...options,
  });
  if (result.error) throw result.error;
  assert.strictEqual(result.status, 0, result.stderr || result.stdout || `${command} failed`);
  return result;
}

function runNpm(args, options = {}) {
  if (process.env.npm_execpath) {
    return run(process.execPath, [process.env.npm_execpath, ...args], options);
  }
  return run(process.platform === 'win32' ? 'npm.cmd' : 'npm', args, {
    shell: process.platform === 'win32',
    ...options,
  });
}

function pack(packagePath, destination) {
  const result = runNpm([
    'pack',
    packagePath,
    '--pack-destination',
    destination,
    '--json',
  ]);
  const metadata = JSON.parse(result.stdout);
  assert.strictEqual(metadata.length, 1);
  return path.join(destination, metadata[0].filename);
}

const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'codeloupe-mcp-packed-install-'));
try {
  const artifacts = path.join(temporaryRoot, 'artifacts');
  const installRoot = path.join(temporaryRoot, 'install');
  fs.mkdirSync(artifacts, { recursive: true });
  fs.mkdirSync(installRoot, { recursive: true });

  const platformManifest = require(path.join(platformPackageRoot, 'package.json'));
  assert.strictEqual(platformManifest.version, mainPackage.version);
  assert.strictEqual(mainPackage.optionalDependencies[platformManifest.name], mainPackage.version);

  const platformTarball = pack(platformPackageRoot, artifacts);
  const launcherTarball = pack(packageRoot, artifacts);
  fs.writeFileSync(path.join(installRoot, 'package.json'), JSON.stringify({
    private: true,
    dependencies: {
      [mainPackage.name]: `file:${launcherTarball.replace(/\\/g, '/')}`,
      [platformManifest.name]: `file:${platformTarball.replace(/\\/g, '/')}`,
    },
  }, null, 2));

  runNpm([
    'install',
    '--ignore-scripts',
    '--no-audit',
    '--no-fund',
    '--package-lock=false',
  ], { cwd: installRoot });

  const environment = { ...process.env };
  delete environment.CODELOUPE_MCP_BINARY;
  delete environment.codeloupe_mcp_BINARY;
  delete environment.CODEBASE_MCP_BINARY;
  const installedLauncher = path.join(
    installRoot,
    'node_modules',
    '@ndhkaeru',
    'codeloupe-mcp',
    'bin',
    'codeloupe-mcp.js'
  );
  const smoke = run(process.execPath, [installedLauncher, '--version'], {
    cwd: installRoot,
    env: environment,
  });
  assert.strictEqual(smoke.stdout.trim(), `codeloupe-mcp ${mainPackage.version}`);
  assert.strictEqual(smoke.stderr.trim(), '');
  console.log(`packed install smoke passed for ${platformKey}`);
} finally {
  fs.rmSync(temporaryRoot, { recursive: true, force: true });
}
