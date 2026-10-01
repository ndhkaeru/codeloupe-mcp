'use strict';

// Windows-only E2E check: upgrading the installed npm packages while a server
// started through the launcher is still running must not hit EBUSY/EPERM or
// leave npm temp directories behind. A copy of sort.exe stands in for the
// native binary because it stays alive until stdin closes.

const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn, spawnSync } = require('child_process');

if (process.platform !== 'win32') {
  console.log('upgrade-while-running test skipped: Windows only');
  process.exit(0);
}

const packageRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(packageRoot, '..', '..');
const platformKey = `${process.platform}-${process.arch}`;
const mainManifest = require(path.join(packageRoot, 'package.json'));
const platformManifest = require(path.join(repositoryRoot, 'packages', 'npm-platforms', platformKey, 'package.json'));
const standIn = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'sort.exe');
const versions = ['0.0.0-upgrade.1', '0.0.0-upgrade.2'];

const temporaryRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'codeloupe-mcp-upgrade-'));
const npmEnvironment = {
  ...process.env,
  npm_config_cache: path.join(temporaryRoot, 'npm-cache'),
  npm_config_update_notifier: 'false',
};

function quote(argument) {
  return /[\s"&|<>^]/.test(argument) ? `"${argument}"` : argument;
}

function npm(args, cwd) {
  const result = spawnSync(['npm', ...args].map(quote).join(' '), {
    cwd,
    env: npmEnvironment,
    encoding: 'utf8',
    shell: true,
  });
  return { status: result.status, output: `${result.stdout || ''}${result.stderr || ''}` };
}

function writeJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function pack(directory) {
  fs.mkdirSync(path.join(temporaryRoot, 'artifacts'), { recursive: true });
  const result = npm(['pack', directory, '--pack-destination', path.join(temporaryRoot, 'artifacts'), '--json'], temporaryRoot);
  assert.strictEqual(result.status, 0, result.output);
  const start = result.output.indexOf('[');
  return path.join(temporaryRoot, 'artifacts', JSON.parse(result.output.slice(start))[0].filename);
}

function buildVersion(version) {
  const stage = path.join(temporaryRoot, 'stage', version);
  const platformDirectory = path.join(stage, 'platform');
  writeJson(path.join(platformDirectory, 'package.json'), { ...platformManifest, version, files: ['bin/'] });
  fs.mkdirSync(path.join(platformDirectory, 'bin'), { recursive: true });
  fs.copyFileSync(standIn, path.join(platformDirectory, 'bin', 'codeloupe-mcp.exe'));

  const launcherDirectory = path.join(stage, 'launcher');
  writeJson(path.join(launcherDirectory, 'package.json'), {
    ...mainManifest,
    version,
    files: ['bin/'],
    optionalDependencies: {},
    scripts: {},
  });
  fs.mkdirSync(path.join(launcherDirectory, 'bin'), { recursive: true });
  fs.copyFileSync(path.join(packageRoot, 'bin', 'codeloupe-mcp.js'), path.join(launcherDirectory, 'bin', 'codeloupe-mcp.js'));

  return { launcher: pack(launcherDirectory), platform: pack(platformDirectory) };
}

function install(installRoot, tarballs) {
  writeJson(path.join(installRoot, 'package.json'), {
    private: true,
    dependencies: {
      [mainManifest.name]: `file:${tarballs.launcher.replace(/\\/g, '/')}`,
      [platformManifest.name]: `file:${tarballs.platform.replace(/\\/g, '/')}`,
    },
  });
  return npm(['install', '--ignore-scripts', '--no-audit', '--no-fund', '--package-lock=false'], installRoot);
}

function sleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function scenario(label, runtimeCopy, builds) {
  const installRoot = path.join(temporaryRoot, label);
  const runtimeDirectory = path.join(temporaryRoot, `${label}-runtime`);
  const first = install(installRoot, builds[0]);
  assert.strictEqual(first.status, 0, first.output);

  const launcherPath = path.join(installRoot, 'node_modules', '@ndhkaeru', 'codeloupe-mcp', 'bin', 'codeloupe-mcp.js');
  const child = spawn(process.execPath, [launcherPath], {
    cwd: installRoot,
    env: {
      ...process.env,
      CODELOUPE_MCP_BINARY: '',
      CODELOUPE_MCP_RUNTIME_COPY: runtimeCopy ? '1' : '0',
      CODELOUPE_MCP_RUNTIME_DIR: runtimeDirectory,
    },
    stdio: ['pipe', 'ignore', 'pipe'],
    windowsHide: true,
  });
  let launcherStderr = '';
  child.stderr.on('data', (chunk) => { launcherStderr += chunk; });
  const exited = new Promise((resolve) => child.on('exit', resolve));

  try {
    await sleep(1500);
    assert.strictEqual(child.exitCode, null, `launcher exited early: ${launcherStderr}`);
    const second = install(installRoot, builds[1]);
    const scope = path.join(installRoot, 'node_modules', '@ndhkaeru');
    const leftovers = fs.readdirSync(scope).filter((name) => name.startsWith('.'));
    const installed = JSON.parse(fs.readFileSync(path.join(scope, `codeloupe-mcp-${platformKey}`, 'package.json'), 'utf8')).version;
    const runtimeCopies = fs.existsSync(runtimeDirectory) ? fs.readdirSync(runtimeDirectory) : [];
    return {
      status: second.status,
      lockError: /EBUSY|EPERM/.test(second.output),
      leftovers,
      installed,
      runtimeCopies,
      output: second.output,
      launcherStderr,
    };
  } finally {
    child.stdin.end();
    const code = await Promise.race([exited, sleep(5000).then(() => 'timeout')]);
    if (code === 'timeout') {
      spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
    }
  }
}

(async () => {
  try {
    const builds = versions.map(buildVersion);

    const withCopy = await scenario('with-runtime-copy', true, builds);
    assert.strictEqual(withCopy.status, 0, withCopy.output);
    assert.strictEqual(withCopy.lockError, false, `npm reported a lock error:\n${withCopy.output}`);
    assert.deepStrictEqual(withCopy.leftovers, [], `npm left temp directories: ${withCopy.leftovers.join(', ')}`);
    assert.strictEqual(withCopy.installed, versions[1]);
    assert.strictEqual(withCopy.runtimeCopies.length, 1, `expected one runtime copy, got ${withCopy.runtimeCopies.join(', ')}`);
    assert.strictEqual(withCopy.launcherStderr.trim(), '');

    const direct = await scenario('direct-binary', false, builds);
    const reproduced = direct.status !== 0 || direct.lockError || direct.leftovers.length > 0;
    console.log(`control without runtime copy: ${reproduced ? 'lock problem reproduced' : 'lock problem not reproduced on this machine'} (exit ${direct.status}, leftovers: ${direct.leftovers.join(', ') || 'none'})`);
    console.log('upgrade-while-running test passed');
  } finally {
    fs.rmSync(temporaryRoot, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
})().catch((error) => {
  console.error(error.stack || error.message);
  process.exit(1);
});
