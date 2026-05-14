#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const SUPPORTED = new Map([
  ['darwin:arm64', 'lterm-darwin-arm64'],
  ['darwin:x64', 'lterm-darwin-x64'],
  ['linux:arm64', 'lterm-linux-arm64'],
  ['linux:x64', 'lterm-linux-x64'],
]);

function isExecutable(file) {
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return true;
  } catch (_) {
    return false;
  }
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
    return process.env.LTERM_BIN;
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

process.exit(result.status === null ? 1 : result.status);
