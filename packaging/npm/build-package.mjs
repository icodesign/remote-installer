#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));

function fail(message) {
  console.error(`build npm package: ${message}`);
  process.exit(1);
}

function requiredOption(args, name) {
  const index = args.indexOf(name);
  if (index < 0 || !args[index + 1] || args[index + 1].startsWith('--')) {
    fail(`missing ${name}`);
  }
  return args[index + 1];
}

function validateVersion(version) {
  // The release workflow derives this from a vX.Y.Z tag. Keep prerelease/build
  // suffixes valid for release candidates while rejecting shell/template input.
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    fail(`invalid release version: ${version}`);
  }
}

function validatePackageName(name) {
  if (!/^(?:@[a-z0-9._~-]+\/)?[a-z0-9._~-]+$/.test(name)) {
    fail(`invalid NPM_PACKAGE_NAME: ${name}`);
  }
}

function copyExecutable(source, destination) {
  let stat;
  try {
    stat = fs.statSync(source);
  } catch {
    fail(`binary does not exist: ${source}`);
  }
  if (!stat.isFile()) {
    fail(`binary path is not a file: ${source}`);
  }
  try {
    fs.accessSync(source, fs.constants.X_OK);
  } catch {
    fail(`binary is not executable: ${source}`);
  }
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.copyFileSync(source, destination);
  fs.chmodSync(destination, 0o755);
}

const args = process.argv.slice(2);
const version = requiredOption(args, '--version');
const arm64Binary = requiredOption(args, '--arm64-binary');
const x64Binary = requiredOption(args, '--x64-binary');
const outputDirectory = path.resolve(requiredOption(args, '--output'));
const packageName = process.env.NPM_PACKAGE_NAME;

if (!packageName) {
  fail('NPM_PACKAGE_NAME is required; set it to the package name that will be published');
}
validatePackageName(packageName);
validateVersion(version);

const templatePath = path.join(root, 'package.json.template');
const template = fs.readFileSync(templatePath, 'utf8');
const packageJson = template
  .replaceAll('@@NPM_PACKAGE_NAME@@', packageName)
  .replaceAll('@@VERSION@@', version);

let parsedPackage;
try {
  parsedPackage = JSON.parse(packageJson);
} catch (error) {
  fail(`generated package.json is invalid: ${error.message}`);
}
if (parsedPackage.name !== packageName || parsedPackage.version !== version) {
  fail('generated package.json did not preserve the configured name and release version');
}

// Only generated package contents are touched. The caller should use a fresh
// dist directory, but removing these known output directories also prevents a
// stale binary from accidentally being included in a repeated local build.
for (const directory of ['bin', 'vendor']) {
  fs.rmSync(path.join(outputDirectory, directory), { recursive: true, force: true });
}
fs.mkdirSync(outputDirectory, { recursive: true });
fs.writeFileSync(
  path.join(outputDirectory, 'package.json'),
  `${JSON.stringify(parsedPackage, null, 2)}\n`,
  'utf8'
);
fs.copyFileSync(path.join(root, 'README.md'), path.join(outputDirectory, 'README.md'));

const launcherPath = path.join(outputDirectory, 'bin', 'remote-installer.js');
fs.mkdirSync(path.dirname(launcherPath), { recursive: true });
fs.copyFileSync(path.join(root, 'bin', 'remote-installer.js'), launcherPath);
fs.chmodSync(launcherPath, 0o755);
copyExecutable(
  arm64Binary,
  path.join(outputDirectory, 'vendor', 'darwin-arm64', 'remote-installer')
);
copyExecutable(
  x64Binary,
  path.join(outputDirectory, 'vendor', 'darwin-x64', 'remote-installer')
);

console.log(`built ${packageName}@${version} in ${outputDirectory}`);
