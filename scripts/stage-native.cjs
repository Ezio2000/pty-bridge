#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const [, , target, source] = process.argv;
const targets = new Set(['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64']);
if (!targets.has(target) || !source) {
  throw new Error('usage: stage-native.cjs <platform-arch> <binary>');
}

const root = path.resolve(__dirname, '..');
const binaryName = target.startsWith('win32-') ? 'pty-bridge.exe' : 'pty-bridge';
const pluginNativeDir = path.join(root, 'packages', 'plugin', 'native', target);
fs.mkdirSync(pluginNativeDir, { recursive: true });
fs.copyFileSync(path.resolve(source), path.join(pluginNativeDir, binaryName));
if (!target.startsWith('win32-')) fs.chmodSync(path.join(pluginNativeDir, binaryName), 0o755);

console.log(`staged ${target}`);
