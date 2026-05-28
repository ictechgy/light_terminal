#!/usr/bin/env node
'use strict';

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, '..');
const requireBinaries = process.argv.includes('--require-binaries');
const platforms = [
  {
    name: 'lterm-darwin-arm64',
    dir: 'npm/platforms/lterm-darwin-arm64',
    os: 'darwin',
    cpu: 'arm64',
  },
  {
    name: 'lterm-darwin-x64',
    dir: 'npm/platforms/lterm-darwin-x64',
    os: 'darwin',
    cpu: 'x64',
  },
  {
    name: 'lterm-linux-arm64',
    dir: 'npm/platforms/lterm-linux-arm64',
    os: 'linux',
    cpu: 'arm64',
  },
  {
    name: 'lterm-linux-x64',
    dir: 'npm/platforms/lterm-linux-x64',
    os: 'linux',
    cpu: 'x64',
  },
];
const rootScripts = {
  prepublishOnly: 'node scripts/validate_npm_packages.mjs',
  'validate:npm-packages': 'node scripts/validate_npm_packages.mjs',
};
const forbiddenPlatformFields = [
  'scripts',
  'dependencies',
  'devDependencies',
  'optionalDependencies',
  'peerDependencies',
  'bundledDependencies',
  'bundleDependencies',
];

function readJson(relativePath) {
  return JSON.parse(fs.readFileSync(path.join(root, relativePath), 'utf8'));
}

function fail(message) {
  console.error(`npm package validation failed: ${message}`);
  process.exitCode = 1;
}

function sameArray(actual, expected) {
  return (
    Array.isArray(actual) &&
    actual.length === expected.length &&
    expected.every((value, index) => actual[index] === value)
  );
}

function sameObject(actual, expected) {
  return JSON.stringify(actual || {}) === JSON.stringify(expected);
}

const rootPackage = readJson('package.json');
const version = rootPackage.version;
const optionalDeps = rootPackage.optionalDependencies || {};
const expectedOptionalDeps = Object.fromEntries(platforms.map((platform) => [platform.name, version]));

if (!sameObject(rootPackage.scripts, rootScripts)) {
  fail(`root scripts ${JSON.stringify(rootPackage.scripts || {})} do not match expected publish guards`);
}

const actualKeys = Object.keys(optionalDeps).sort();
const expectedKeys = Object.keys(expectedOptionalDeps).sort();
if (JSON.stringify(actualKeys) !== JSON.stringify(expectedKeys)) {
  fail(`root optionalDependencies keys ${JSON.stringify(actualKeys)} do not match ${JSON.stringify(expectedKeys)}`);
}
for (const [name, expectedVersion] of Object.entries(expectedOptionalDeps)) {
  if (optionalDeps[name] !== expectedVersion) {
    fail(`root optionalDependency ${name}@${optionalDeps[name]} does not match root version ${expectedVersion}`);
  }
}

for (const platform of platforms) {
  const pkg = readJson(`${platform.dir}/package.json`);
  if (pkg.name !== platform.name) {
    fail(`${platform.dir} package name ${pkg.name} does not match ${platform.name}`);
  }
  if (pkg.version !== version) {
    fail(`${platform.name} version ${pkg.version} does not match root version ${version}`);
  }
  if (!sameArray(pkg.os, [platform.os])) {
    fail(`${platform.name} os ${JSON.stringify(pkg.os)} does not match [${platform.os}]`);
  }
  if (!sameArray(pkg.cpu, [platform.cpu])) {
    fail(`${platform.name} cpu ${JSON.stringify(pkg.cpu)} does not match [${platform.cpu}]`);
  }
  if (!sameArray(pkg.files, ['bin/lterm'])) {
    fail(`${platform.name} files ${JSON.stringify(pkg.files)} does not exactly publish bin/lterm`);
  }
  for (const field of forbiddenPlatformFields) {
    if (Object.prototype.hasOwnProperty.call(pkg, field)) {
      fail(`${platform.name} must not declare ${field}`);
    }
  }
  if (requireBinaries) {
    const binary = path.join(root, platform.dir, 'bin', 'lterm');
    try {
      fs.accessSync(binary, fs.constants.X_OK);
    } catch (_) {
      fail(`${platform.name} missing executable ${path.relative(root, binary)}`);
    }
  }
}

if (process.exitCode) {
  process.exit(process.exitCode);
}
console.log(`npm package metadata ok for ${rootPackage.name}@${version}`);
