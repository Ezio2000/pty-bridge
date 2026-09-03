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
const packageDir = path.join(root, 'packages', target);
const binaryName = target.startsWith('win32-') ? 'pty-bridge.exe' : 'pty-bridge';
const binDir = path.join(packageDir, 'bin');
fs.mkdirSync(binDir, { recursive: true });
fs.copyFileSync(path.resolve(source), path.join(binDir, binaryName));
if (!target.startsWith('win32-')) fs.chmodSync(path.join(binDir, binaryName), 0o755);

const pluginNativeDir = path.join(root, 'packages', 'plugin', 'native', target);
fs.mkdirSync(pluginNativeDir, { recursive: true });
fs.copyFileSync(path.resolve(source), path.join(pluginNativeDir, binaryName));
if (!target.startsWith('win32-')) fs.chmodSync(path.join(pluginNativeDir, binaryName), 0o755);

fs.copyFileSync(path.join(root, 'LICENSE'), path.join(packageDir, 'LICENSE'));
fs.writeFileSync(
  path.join(packageDir, 'README.md'),
  `# @pty-bridge/${target}\n\nNative ${target} binary for [@pty-bridge/plugin](https://github.com/Ezio2000/pty-bridge).\n`
);

console.log(`staged ${target}`);
