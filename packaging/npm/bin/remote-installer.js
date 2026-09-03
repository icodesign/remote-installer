#!/usr/bin/env node

'use strict';

const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

if (process.platform !== 'darwin') {
  console.error(
    'The published remote-installer package currently provides binaries for macOS (darwin) only.'
  );
  process.exit(1);
}

const platformDirectory = {
  arm64: 'darwin-arm64',
  x64: 'darwin-x64',
}[process.arch];

if (!platformDirectory) {
  console.error(
    `remote-installer does not provide a macOS binary for architecture ${process.arch}. ` +
      'Supported architectures are arm64 and x64.'
  );
  process.exit(1);
}

const binaryPath = path.join(__dirname, '..', 'vendor', platformDirectory, 'remote-installer');

try {
  fs.accessSync(binaryPath, fs.constants.X_OK);
} catch {
  console.error(`remote-installer binary is missing or not executable: ${binaryPath}`);
  console.error('Reinstall the package or report the broken package version.');
  process.exit(1);
}

// Put the native CLI and the tunnel processes it creates in their own process
// group. The Node launcher then owns forwarding termination signals to that
// whole group, including when an agent stops the launcher by PID instead of a
// terminal sending Ctrl-C to every foreground process.
const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  detached: true,
});

let spawnFailed = false;
child.on('error', (error) => {
  spawnFailed = true;
  removeSignalHandlers();
  console.error(`failed to start remote-installer: ${error.message}`);
  process.exitCode = 1;
});

const forwardedSignals = ['SIGINT', 'SIGTERM', 'SIGHUP'];
function forwardSignal(signal) {
  if (child.pid == null) {
    return;
  }
  try {
    process.kill(-child.pid, signal);
  } catch (error) {
    if (error.code !== 'ESRCH') {
      console.error(`failed to forward ${signal} to remote-installer: ${error.message}`);
      process.exitCode = 1;
    }
  }
}

const signalHandlers = new Map();
for (const signal of forwardedSignals) {
  const handler = () => forwardSignal(signal);
  signalHandlers.set(signal, handler);
  process.on(signal, handler);
}

function removeSignalHandlers() {
  for (const [forwardedSignal, handler] of signalHandlers) {
    process.off(forwardedSignal, handler);
  }
}

child.on('exit', (code, signal) => {
  removeSignalHandlers();
  if (spawnFailed) {
    return;
  }
  if (signal) {
    // Preserve the native CLI's signal-based exit status for callers and agents.
    process.kill(process.pid, signal);
    return;
  }
  process.exitCode = code == null ? 1 : code;
});
