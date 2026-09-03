#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');

const supported = new Set([
  'darwin-arm64',
  'darwin-x64',
  'linux-arm64',
  'linux-x64',
  'win32-arm64',
  'win32-x64'
]);

function resolveBinary() {
  if (process.env.PTY_BRIDGE_BIN) {
    return process.env.PTY_BRIDGE_BIN;
  }
  const target = `${process.platform}-${process.arch}`;
  if (!supported.has(target)) {
    throw new Error(`Unsupported platform: ${target}`);
  }
  const binaryName = process.platform === 'win32' ? 'pty-bridge.exe' : 'pty-bridge';
  const bundled = path.resolve(__dirname, '..', 'native', target, binaryName);
  if (fs.existsSync(bundled)) {
    return bundled;
  }
  const local = path.resolve(__dirname, '..', '..', '..', 'target', 'debug', binaryName);
  if (fs.existsSync(local)) return local;
  throw new Error(`Bundled native binary is missing for ${target}. Reinstall @pty-bridge/plugin.`);
}

let binary;
try {
  binary = resolveBinary();
} catch (error) {
  console.error(`[pty-bridge] ${error.message}`);
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true,
  env: process.env
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}

child.on('error', (error) => {
  console.error(`[pty-bridge] failed to launch native binary: ${error.message}`);
  process.exitCode = 1;
});

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exitCode = code ?? 1;
});
