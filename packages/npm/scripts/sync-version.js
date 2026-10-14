'use strict';

const fs = require('fs');
const path = require('path');
const { randomUUID } = require('crypto');

const repositoryRoot = path.resolve(__dirname, '..', '..', '..');
const platforms = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64'];
const identifier = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)';
const semver = new RegExp('^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)' +
  '(?:-(' + identifier + '(?:\\.' + identifier + ')*))?(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$');

function assignment(block, key) {
  const expression = new RegExp('^([ \\t]*' + key + '[ \\t]*=[ \\t]*)(["\\\'])([^"\\\'\\r\\n]*)\\2([ \\t]*(?:#[^\\r\\n]*)?\\r?)$', 'gm');
  const matches = [...block.matchAll(expression)];
  if (matches.length !== 1) throw new Error('Expected one literal ' + key + ' assignment in the package section');
  return matches[0];
}

function replaceAssignment(block, match, value) {
  return block.slice(0, match.index) + match[1] + match[2] + value + match[2] + match[4] +
    block.slice(match.index + match[0].length);
}

function blocks(text, expression) {
  const headers = [...text.matchAll(expression)];
  return headers.map((header, index) => ({
    start: header.index,
    end: index + 1 < headers.length ? headers[index + 1].index : text.length,
    header: header[0].trim(),
  }));
}

function cargoPackage(text) {
  const sections = blocks(text, /^[ \t]*\[[^\r\n]+\][ \t]*(?:#[^\r\n]*)?\r?$/gm);
  const packages = sections.filter((section) => /^\[package\][ \t]*(?:#.*)?$/.test(section.header));
  if (packages.length !== 1) throw new Error('Cargo.toml must contain one [package] section');
  const section = packages[0];
  const block = text.slice(section.start, section.end);
  return { name: assignment(block, 'name')[3], version: assignment(block, 'version')[3] };
}

function syncLock(text, name, version) {
  const packages = blocks(text, /^[ \t]*\[\[package\]\][ \t]*(?:#[^\r\n]*)?\r?$/gm);
  const matching = packages.filter((section) => assignment(text.slice(section.start, section.end), 'name')[3] === name);
  if (matching.length !== 1) throw new Error('Cargo.lock must contain exactly one root package named ' + name);
  const section = matching[0];
  const block = text.slice(section.start, section.end);
  return text.slice(0, section.start) + replaceAssignment(block, assignment(block, 'version'), version) +
    text.slice(section.end);
}

// Tokenize strings so quoted examples inside descriptions cannot be mistaken for keys.
function jsonStrings(text, values, expectedCounts) {
  const tokens = [...text.matchAll(/"(?:\\.|[^"\\])*"/g)];
  const counts = Object.fromEntries(Object.keys(values).map((key) => [key, 0]));
  const edits = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    const key = JSON.parse(token[0]);
    if (!Object.hasOwn(values, key)) continue;
    const end = token.index + token[0].length;
    const prefix = /^[ \t\r\n]*:[ \t\r\n]*/.exec(text.slice(end));
    if (!prefix) continue;
    const value = tokens[index + 1];
    if (!value || value.index !== end + prefix[0].length) {
      throw new Error('Expected a string value for ' + key);
    }
    counts[key] += 1;
    edits.push({ start: value.index, end: value.index + value[0].length, text: JSON.stringify(values[key]) });
  }
  for (const [key, expected] of Object.entries(expectedCounts)) {
    if (counts[key] !== expected) throw new Error('Expected ' + expected + ' ' + key + ' fields, found ' + counts[key]);
  }
  for (const edit of edits.reverse()) text = text.slice(0, edit.start) + edit.text + text.slice(edit.end);
  JSON.parse(text);
  return text;
}

function atomicWrite(filename, content) {
  const temporary = path.join(path.dirname(filename), '.' + path.basename(filename) + '.version-' + randomUUID() + '.tmp');
  const descriptor = fs.openSync(temporary, 'wx', fs.statSync(filename).mode & 0o777);
  let closed = false;
  try {
    fs.writeFileSync(descriptor, content, 'utf8');
    fs.fsyncSync(descriptor);
    fs.closeSync(descriptor);
    closed = true;
    fs.renameSync(temporary, filename);
  } finally {
    if (!closed) fs.closeSync(descriptor);
    fs.rmSync(temporary, { force: true });
  }
}

function syncVersion(root = repositoryRoot, options = {}) {
  const read = (relative) => {
    const filename = path.join(root, relative);
    const metadata = fs.lstatSync(filename);
    if (!metadata.isFile() || metadata.isSymbolicLink()) throw new Error(relative + ' must be a regular file');
    return { relative, filename, before: fs.readFileSync(filename, 'utf8') };
  };
  const cargoFile = read('Cargo.toml');
  const cargo = cargoPackage(cargoFile.before);
  if (cargo.name !== 'codeloupe-mcp') throw new Error('Unexpected Cargo package name: ' + cargo.name);
  if (!semver.test(cargo.version)) throw new Error('Cargo.toml contains an invalid semantic version: ' + cargo.version);
  const planned = [];
  const add = (file, after) => planned.push({ ...file, after });

  const lock = read('Cargo.lock');
  add(lock, syncLock(lock.before, cargo.name, cargo.version));
  const launcher = read('packages/npm/package.json');
  const main = JSON.parse(launcher.before);
  if (main.name !== '@ndhkaeru/codeloupe-mcp') throw new Error('Unexpected npm launcher name');
  if (typeof main.mcpName !== 'string' || !main.mcpName.trim()) throw new Error('Missing npm mcpName');
  if (!main.optionalDependencies || Object.keys(main.optionalDependencies).length !== platforms.length) {
    throw new Error('Expected exactly six native optional dependencies');
  }
  const values = { version: cargo.version };
  const counts = { version: 1 };
  for (const platform of platforms) {
    const name = '@ndhkaeru/codeloupe-mcp-' + platform;
    if (!Object.hasOwn(main.optionalDependencies, name) || typeof main.optionalDependencies[name] !== 'string') {
      throw new Error('Missing native optional dependency: ' + name);
    }
    values[name] = cargo.version;
    counts[name] = 1;
  }
  add(launcher, jsonStrings(launcher.before, values, counts));

  for (const platform of platforms) {
    const file = read('packages/npm-platforms/' + platform + '/package.json');
    const manifest = JSON.parse(file.before);
    const [os, cpu] = platform.split('-');
    if (manifest.name !== '@ndhkaeru/codeloupe-mcp-' + platform ||
      JSON.stringify(manifest.os) !== JSON.stringify([os]) || JSON.stringify(manifest.cpu) !== JSON.stringify([cpu])) {
      throw new Error('Unexpected identity or compatibility for ' + platform);
    }
    add(file, jsonStrings(file.before, { version: cargo.version }, { version: 1 }));
  }
  const serverFile = read('server.json');
  const server = JSON.parse(serverFile.before);
  if (server.name !== main.mcpName || !Array.isArray(server.packages) || server.packages.length !== 1 ||
    server.packages[0].registryType !== 'npm' || server.packages[0].identifier !== main.name) {
    throw new Error('MCP registry identity must match the npm launcher');
  }
  add(serverFile, jsonStrings(serverFile.before, { version: cargo.version }, { version: 2 }));
  const changed = planned.filter((file) => file.before !== file.after);
  if (options.check) {
    if (changed.length) throw new Error('Version ' + cargo.version + ' is out of sync:\n' +
      changed.map((file) => '  ' + file.relative).join('\n') + '\nRun npm --prefix packages/npm run version:sync');
    return { version: cargo.version, changed: [] };
  }

  const write = options.writeFile || atomicWrite;
  const written = [];
  try {
    for (const file of changed) {
      if (fs.readFileSync(cargoFile.filename, 'utf8') !== cargoFile.before ||
        fs.readFileSync(file.filename, 'utf8') !== file.before) {
        throw new Error('Concurrent change detected before writing ' + file.relative);
      }
      write(file.filename, file.after);
      written.push(file);
    }
  } catch (error) {
    const failures = [];
    for (const file of written.reverse()) {
      try {
        if (fs.readFileSync(file.filename, 'utf8') !== file.after) {
          throw new Error('File changed independently; refusing to overwrite it');
        }
        write(file.filename, file.before);
      } catch (rollbackError) {
        failures.push(file.relative + ': ' + rollbackError.message);
      }
    }
    throw new Error(error.message + (failures.length ? '\nRollback incomplete:\n' + failures.join('\n') : ''));
  }
  return { version: cargo.version, changed: changed.map((file) => file.relative) };
}

if (require.main === module) {
  try {
    const args = process.argv.slice(2);
    if (args.length && (args.length !== 1 || args[0] !== '--check')) {
      throw new Error('Usage: node packages/npm/scripts/sync-version.js [--check]');
    }
    const result = syncVersion(repositoryRoot, { check: args[0] === '--check' });
    console.log('Version ' + result.version + ': ' + (args[0] === '--check' ?
      'all metadata is synchronized' : result.changed.length + ' metadata files updated'));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}

module.exports = { syncVersion };
