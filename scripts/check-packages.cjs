'use strict';

const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const pluginDir = path.join(root, 'packages', 'plugin');
const plugin = require(path.join(pluginDir, 'package.json'));
const manifest = require(path.join(pluginDir, '.claude-plugin', 'plugin.json'));
const hooks = require(path.join(pluginDir, 'hooks', 'hooks.json'));
const workspace = require(path.join(root, 'package.json'));
const cargo = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');

if (workspace.version !== plugin.version) throw new Error('Workspace/plugin version mismatch');
if (!cargo.includes(`version = "${plugin.version}"`)) throw new Error('Cargo/npm version mismatch');
if (manifest.version !== plugin.version) throw new Error('Plugin manifest/npm version mismatch');
if (!plugin.files.includes('native')) throw new Error('Plugin must include native binaries');
if (plugin.files.includes('skills')) throw new Error('Plugin must not include a Skill');
if (fs.existsSync(path.join(pluginDir, 'skills'))) throw new Error('Legacy Skill directory still exists');
if (plugin.optionalDependencies) throw new Error('Plugin must be a single self-contained package');
if (fs.readFileSync(path.join(pluginDir, 'LICENSE'), 'utf8') !== fs.readFileSync(path.join(root, 'LICENSE'), 'utf8')) {
  throw new Error('Plugin license must match the repository license');
}

const expectedMatcher = 'mcp__plugin_pty-bridge_pty__start';
if (hooks.hooks.PostToolUse?.[0]?.matcher !== expectedMatcher) {
  throw new Error('PostToolUse hook must match the plugin-qualified start tool');
}
if (hooks.hooks.SessionEnd?.[0]?.hooks?.[0]?.timeout !== 1) {
  throw new Error('SessionEnd hook must stay inside the host shutdown budget');
}

const forbidden = ['claude-code-pty', 'claude_pty'];
for (const file of [
  path.join(pluginDir, '.mcp.json'),
  path.join(pluginDir, 'hooks', 'hooks.json'),
  path.join(pluginDir, 'launcher', 'launch.cjs')
]) {
  const content = fs.readFileSync(file, 'utf8').toLowerCase();
  for (const value of forbidden) {
    if (content.includes(value)) throw new Error(`Legacy identifier ${value} in ${file}`);
  }
}

const releaseWorkflow = fs.readFileSync(path.join(root, '.github', 'workflows', 'release.yml'), 'utf8');
if (releaseWorkflow.includes('packages/${{ matrix.package }}/bin')) {
  throw new Error('Release workflow still uploads deleted platform subpackages');
}
if (!releaseWorkflow.includes('packages/plugin/native/${{ matrix.package }}/*')) {
  throw new Error('Release workflow must upload the self-contained plugin native artifact');
}

if (process.argv.includes('--release')) {
  const targets = ['darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-arm64', 'win32-x64'];
  for (const target of targets) {
    const binary = target.startsWith('win32-') ? 'pty-bridge.exe' : 'pty-bridge';
    const file = path.join(pluginDir, 'native', target, binary);
    const stat = fs.statSync(file);
    if (!stat.isFile() || stat.size < 1024 * 1024) throw new Error(`Invalid release binary: ${file}`);
  }
}

console.log(`validated self-contained @pty-bridge/plugin ${plugin.version}`);
