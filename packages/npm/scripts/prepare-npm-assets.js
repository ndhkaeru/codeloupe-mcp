#!/usr/bin/env node
'use strict';

const fs = require('fs');
const https = require('https');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const OWNER = process.env.GITHUB_REPOSITORY_OWNER || 'ndhkaeru';
const REPO = (process.env.GITHUB_REPOSITORY || 'ndhkaeru/codeloupe-mcp').split('/')[1] || 'codeloupe-mcp';
const TAG = process.env.GITHUB_REF_NAME || process.argv[2] || 'latest';
const PACKAGE_ROOT = path.resolve(__dirname, '..');
const REPOSITORY_ROOT = path.resolve(PACKAGE_ROOT, '..', '..');
const NATIVE_DIR = path.join(PACKAGE_ROOT, 'native');
const PLATFORM_PACKAGES_ROOT = path.resolve(PACKAGE_ROOT, '..', 'npm-platforms');
const PLATFORM_README = path.join(PLATFORM_PACKAGES_ROOT, 'README.md');
const LICENSE = path.join(REPOSITORY_ROOT, 'LICENSE');
const MAIN_PACKAGE = require(path.join(PACKAGE_ROOT, 'package.json'));

const TARGETS = [
  ['darwin-arm64', 'aarch64-apple-darwin', 'codeloupe-mcp'],
  ['darwin-x64', 'x86_64-apple-darwin', 'codeloupe-mcp'],
  ['linux-arm64', 'aarch64-unknown-linux-musl', 'codeloupe-mcp'],
  ['linux-x64', 'x86_64-unknown-linux-musl', 'codeloupe-mcp'],
  ['win32-arm64', 'aarch64-pc-windows-msvc', 'codeloupe-mcp.exe'],
  ['win32-x64', 'x86_64-pc-windows-msvc', 'codeloupe-mcp.exe'],
];

function requestJson(url) {
  return new Promise((resolve, reject) => {
    const headers = {
      'User-Agent': 'codeloupe-mcp-release-packager',
      'Accept': 'application/vnd.github+json',
    };
    if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    https.get(url, { headers }, (response) => {
      if (response.statusCode < 200 || response.statusCode >= 300) {
        reject(new Error(`GET ${url} failed: ${response.statusCode}`));
        response.resume();
        return;
      }
      let body = '';
      response.setEncoding('utf8');
      response.on('data', (chunk) => body += chunk);
      response.on('end', () => resolve(JSON.parse(body)));
    }).on('error', reject);
  });
}

function download(url, outputPath) {
  return new Promise((resolve, reject) => {
    const headers = { 'User-Agent': 'codeloupe-mcp-release-packager' };
    if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
    https.get(url, { headers }, (response) => {
      if ([301, 302, 303, 307, 308].includes(response.statusCode)) {
        download(response.headers.location, outputPath).then(resolve, reject);
        return;
      }
      if (response.statusCode < 200 || response.statusCode >= 300) {
        reject(new Error(`download failed: ${response.statusCode} ${url}`));
        response.resume();
        return;
      }
      const file = fs.createWriteStream(outputPath);
      response.pipe(file);
      file.on('finish', () => file.close(resolve));
      file.on('error', reject);
    }).on('error', reject);
  });
}

function listFiles(dir) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...listFiles(full));
    else out.push(full);
  }
  return out;
}

function extract(archivePath, outputDir) {
  fs.mkdirSync(outputDir, { recursive: true });
  if (archivePath.endsWith('.zip')) {
    if (process.platform === 'win32') {
      execFileSync('powershell', ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${archivePath.replace(/'/g, "''")}' -DestinationPath '${outputDir.replace(/'/g, "''")}' -Force`], { stdio: 'inherit' });
    } else {
      execFileSync('unzip', ['-q', archivePath, '-d', outputDir], { stdio: 'inherit' });
    }
    return;
  }
  execFileSync('tar', ['-xf', archivePath, '-C', outputDir], { stdio: 'inherit' });
}

async function main() {
  fs.rmSync(NATIVE_DIR, { recursive: true, force: true });

  const releaseUrl = TAG === 'latest'
    ? `https://api.github.com/repos/${OWNER}/${REPO}/releases/latest`
    : `https://api.github.com/repos/${OWNER}/${REPO}/releases/tags/${TAG}`;
  const release = await requestJson(releaseUrl);

  for (const [platformKey, triple, exeName] of TARGETS) {
    const packageDir = path.join(PLATFORM_PACKAGES_ROOT, platformKey);
    const packageJsonPath = path.join(packageDir, 'package.json');
    const platformPackage = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
    const expectedPackageName = `@ndhkaeru/codeloupe-mcp-${platformKey}`;
    if (platformPackage.name !== expectedPackageName) {
      throw new Error(`${packageJsonPath} has name ${platformPackage.name}; expected ${expectedPackageName}`);
    }
    if (platformPackage.version !== MAIN_PACKAGE.version) {
      throw new Error(`${platformPackage.name} version ${platformPackage.version} does not match ${MAIN_PACKAGE.version}`);
    }
    if (MAIN_PACKAGE.optionalDependencies?.[platformPackage.name] !== MAIN_PACKAGE.version) {
      throw new Error(`${platformPackage.name} is missing from optionalDependencies at ${MAIN_PACKAGE.version}`);
    }

    const asset = release.assets.find((item) =>
      item.name.includes(triple) && (item.name.endsWith('.zip') || item.name.endsWith('.tar.xz') || item.name.endsWith('.tar.gz'))
    );
    if (!asset) throw new Error(`Missing release asset for ${triple}`);

    const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), `codeloupe-mcp-${triple}-`));
    const archivePath = path.join(tempDir, asset.name);
    await download(asset.browser_download_url, archivePath);
    extract(archivePath, tempDir);

    const binary = listFiles(tempDir).find((file) => path.basename(file) === exeName);
    if (!binary) throw new Error(`Could not find ${exeName} in ${asset.name}`);

    const outDir = path.join(packageDir, 'bin');
    fs.rmSync(outDir, { recursive: true, force: true });
    fs.mkdirSync(outDir, { recursive: true });
    const outPath = path.join(outDir, exeName);
    fs.copyFileSync(binary, outPath);
    if (process.platform !== 'win32') fs.chmodSync(outPath, 0o755);
    fs.copyFileSync(LICENSE, path.join(packageDir, 'LICENSE'));
    fs.copyFileSync(PLATFORM_README, path.join(packageDir, 'README.md'));
    console.log(`prepared ${platformKey}: ${outPath}`);
  }
}

main().catch((error) => {
  console.error(error.stack || error.message);
  process.exit(1);
});
