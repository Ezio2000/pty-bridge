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
  const packageName = `@pty-bridge/${target}`;
  try {
    const manifest = require.resolve(`${packageName}/package.json`);
    return path.join(path.dirname(manifest), 'bin', process.platform === 'win32' ? 'pty-bridge.exe' : 'pty-bridge');
  } catch (error) {
    const local = path.resolve(__dirname, '..', '..', '..', 'target', 'debug', process.platform === 'win32' ? 'pty-bridge.exe' : 'pty-bridge');
    if (fs.existsSync(local)) return local;
    throw new Error(`Native package ${packageName} is missing. Reinstall @pty-bridge/plugin for ${target}.`, { cause: error });
  }
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
