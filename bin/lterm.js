#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const SUPPORTED = new Map([
  ['darwin:arm64', 'lterm-darwin-arm64'],
  ['darwin:x64', 'lterm-darwin-x64'],
  ['linux:arm64', 'lterm-linux-arm64'],
  ['linux:x64', 'lterm-linux-x64'],
]);

function isExecutable(file) {
  try {
    if (!fs.statSync(file).isFile()) {
      return false;
    }
    fs.accessSync(file, fs.constants.X_OK);
    return true;
  } catch (_) {
    return false;
  }
}

function hasControlChars(value) {
  return /[\x00-\x1f\x7f-\x9f]/.test(value);
}

function overrideBinary(value) {
  if (hasControlChars(value)) {
    throw new Error('LTERM_BIN must not contain control characters.');
  }
  const resolved = path.resolve(value);
  if (!isExecutable(resolved)) {
    throw new Error(`LTERM_BIN is not executable: ${resolved}`);
  }
  return resolved;
}

function packageBinary(packageName) {
  try {
    const packageJson = require.resolve(`${packageName}/package.json`, {
      paths: [__dirname],
    });
    return path.join(path.dirname(packageJson), 'bin', 'lterm');
  } catch (_) {
    return undefined;
  }
}

function repoFallbackBinary() {
  return path.resolve(__dirname, '..', 'target', 'release', 'lterm');
}

function resolveBinary() {
  if (process.env.LTERM_BIN) {
    return overrideBinary(process.env.LTERM_BIN);
  }

  const key = `${process.platform}:${process.arch}`;
  const packageName = SUPPORTED.get(key);
  if (!packageName) {
    throw new Error(
      `Unsupported platform ${process.platform}/${process.arch}. ` +
        'Install from source with `cargo install light-terminal` or use Homebrew on supported Unix platforms.'
    );
  }

  const fromPackage = packageBinary(packageName);
  if (fromPackage && isExecutable(fromPackage)) {
    return fromPackage;
  }

  const fallback = repoFallbackBinary();
  if (isExecutable(fallback)) {
    return fallback;
  }

  throw new Error(
    `Missing native binary package ${packageName}. ` +
      'Reinstall with optional dependencies enabled, or set LTERM_BIN to a built lterm binary.'
  );
}

let binary;
try {
  binary = resolveBinary();
} catch (error) {
  console.error(`lterm npm wrapper: ${error.message}`);
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`lterm npm wrapper: failed to execute ${binary}: ${result.error.message}`);
  process.exit(1);
}

if (result.status !== null) {
  process.exit(result.status);
}

const signalNumber = result.signal ? os.constants.signals[result.signal] : undefined;
process.exit(typeof signalNumber === 'number' ? 128 + signalNumber : 1);
