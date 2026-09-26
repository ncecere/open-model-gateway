#!/usr/bin/env node
// Local, copy-and-own registry installation. No network registry is consulted.
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { resolve, dirname, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const source = resolve(process.argv[2] ?? '/Users/nicholascecere/projects/typescript/bitop-ui');
const selected = ['app-shell', 'breadcrumbs', 'table', 'card', 'button', 'page-header', 'stat-card', 'input', 'field', 'badge', 'empty-state', 'command-palette', 'tabs'];
const registryText = await readFile(resolve(source, 'registry.json'), 'utf8');
const registry = JSON.parse(registryText);
const hash = text => createHash('sha256').update(text).digest('hex');
const seen = new Set();
const files = [];
const dependencies = new Set();
async function copy(name) {
  if (seen.has(name)) return;
  const item = registry.items.find(item => item.name === name);
  if (!item) throw new Error(`Missing local registry item: ${name}`);
  seen.add(name);
  for (const dep of item.registryDependencies ?? []) {
    if (!dep.startsWith('@bitop/')) throw new Error(`Unsupported dependency: ${dep}`);
    await copy(dep.slice(7));
  }
  for (const dep of item.dependencies ?? []) dependencies.add(dep);
  for (const file of item.files ?? []) {
    if (!file.path.startsWith('registry/bitop/') || file.path.includes('..')) throw new Error('Unsafe registry source');
    const target = file.target.replace(/^@ui\//, 'apps/web/src/components/ui/').replace(/^@lib\//, 'apps/web/src/lib/');
    if (!target.startsWith('apps/web/src/') || target.includes('..')) throw new Error('Unsafe registry target');
    const original = await readFile(resolve(source, file.path), 'utf8');
    // Preserve source except import aliases and the recorded popup composition hook.
    let copied = original.replaceAll('@/registry/bitop/ui/', '@/components/ui/').replaceAll('@/registry/bitop/lib/', '@/lib/');
    const patches = [];
    if (name === 'command-palette' && file.path.endsWith('.tsx')) {
      for (const [from, to] of [
        ['export type CommandPaletteProps = {', 'export type CommandPaletteProps = {\n  /** Gateway composition hook for scoped accessible contrast. */\n  className?: string;'],
        ['export function CommandPalette({\n', 'export function CommandPalette({\n  className,\n'],
        ['className={styles.popup} aria-label={label}', 'className={cx(styles.popup, className)} aria-label={label}'],
      ]) {
        if (copied.split(from).length !== 2) throw new Error(`Review command-palette patch against upstream: ${from}`);
        copied = copied.replace(from, to);
      }
      patches.push('optional-popup-className');
    }
    await mkdir(dirname(resolve(root, target)), { recursive: true });
    await writeFile(resolve(root, target), copied);
    files.push({ source: file.path, target, sourceSha256: hash(original), copiedSha256: hash(copied), ...(patches.length ? { patches } : {}) });
  }
}
for (const name of selected) await copy(name);
const manifest = { source: 'Local bitop-ui checkout (copy-and-own; no network registry)', registrySha256: hash(registryText), selected, items: [...seen].sort(), dependencies: [...dependencies].sort(), files: files.sort((a, b) => a.source.localeCompare(b.source)) };
await writeFile(resolve(root, 'apps/web/bitop-provenance.json'), JSON.stringify(manifest, null, 2) + '\n');
console.log(`Copied ${files.length} files from ${relative(root, source)}; dependencies: ${[...dependencies].join(', ')}`);
