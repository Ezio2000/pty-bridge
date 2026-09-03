'use strict';

const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const expectedVersion = require(path.join(root, 'packages/plugin/package.json')).version;
const cargo = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
if (!cargo.includes(`version = "${expectedVersion}"`)) throw new Error('Cargo/npm version mismatch');
const targets = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64'];

for (const target of targets) {
  const pkg = require(path.join(root, 'packages', target, 'package.json'));
  if (pkg.name !== `@pty-bridge/${target}`) throw new Error(`Unexpected package name for ${target}`);
  if (pkg.version !== expectedVersion) throw new Error(`Version mismatch for ${target}`);
  if (pkg.repository?.url !== 'git+https://github.com/Ezio2000/pty-bridge.git') {
    throw new Error(`Repository metadata missing for ${target}`);
  }
}

const plugin = require(path.join(root, 'packages/plugin/package.json'));
const manifest = require(path.join(root, 'packages/plugin/.claude-plugin/plugin.json'));
const hooks = require(path.join(root, 'packages/plugin/hooks/hooks.json'));
if (manifest.version !== expectedVersion) throw new Error('Plugin manifest/npm version mismatch');
if (!plugin.files.includes('native')) throw new Error('Plugin package must include bundled native binaries');
if (!plugin.files.includes('skills')) throw new Error('Plugin package must include its PTY skill');
if (!fs.existsSync(path.join(root, 'packages/plugin/skills/pty/SKILL.md'))) {
  throw new Error('PTY workflow skill is missing');
}
if (hooks.hooks.PostToolUse?.[0]?.matcher !== 'mcp__plugin_pty-bridge_pty__start') {
  throw new Error('PostToolUse hook must match the plugin-qualified PTY start tool');
}
for (const target of targets) {
  if (plugin.optionalDependencies[`@pty-bridge/${target}`] !== expectedVersion) {
    throw new Error(`Optional dependency version mismatch for ${target}`);
  }
}

console.log(`validated @pty-bridge packages at ${expectedVersion}`);
