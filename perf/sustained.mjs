#!/usr/bin/env node
// Sustained-capability evidence for jsrt — scenarios load.py doesn't cover.
//   node perf/sustained.mjs
// kv: 1M get/put ops through rt.kv in one run. media: streamed puts of real files.
// refusal: spawns hitting a denied capability -> exit 7. supervise: crash-loop
// workers each bounded by the supervisor. Dev observations on this machine.
import { spawnSync } from 'node:child_process';
import { writeFileSync, mkdirSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const JSRT = `${ROOT}/target/release/jsrt`;
const tmp = mkdtempSync(join(tmpdir(), 'jsrt-sus-'));
const now = () => performance.now();
const out = { date: new Date().toISOString(), machine: 'apple-m2-pro-12c' };
const sh = (name, code) => { const p = join(tmp, name); writeFileSync(p, code); return p; };

// --- kv sustained: 500k put+get pairs = 1M kv ops ----------------------------------
const kvJs = sh('kv.js', `
for (let i = 0; i < 500000; i++) {
  rt.kv.put('k' + (i % 10000), 'v' + i);
  if (rt.kv.get('k' + (i % 10000)) === null) throw new Error('lost write ' + i);
}
rt.print('keys ' + rt.kv.list('k9').length);
`);
let t = now();
const kv = spawnSync(JSRT, ['run', '--timeout', '0', '--kv', join(tmp, 'app.kv'), kvJs], { encoding: 'utf8', timeout: 600000 });
out.kv = { ops: 1_000_000, seconds: +((now() - t) / 1000).toFixed(1), exit: kv.status, stdout: kv.stdout.trim(), ok: kv.status === 0 };

// --- media sustained: puts of real files through the local backend ------------------
const blobDir = join(tmp, 'blobs'); mkdirSync(blobDir);
writeFileSync(join(blobDir, 'b0'), Buffer.alloc(8192, 7));
writeFileSync(join(blobDir, 'b1'), Buffer.alloc(8192, 9));
const medJs = sh('media.js', `
let names = [];
for (let i = 0; i < 20000; i++) {
  const r = rt.media.put('${blobDir}/b' + (i % 2));
  if (!r || !r.name) throw new Error('put failed ' + i);
  if (i < 20) names.push(r.name);
}
for (const n of names) { const s = rt.media.stat(n); if (!s || s.bytes !== 8192) throw new Error('stat mismatch ' + n); }
rt.print('media ok ' + names[0]);
`);
t = now();
const med = spawnSync(JSRT, ['run', '--timeout', '0', '--read', blobDir, '--media-root', join(tmp, 'media'), medJs], { encoding: 'utf8', timeout: 600000 });
out.media = { puts: 20_000, stats: 20, seconds: +((now() - t) / 1000).toFixed(1), exit: med.status, stdout: med.stdout.trim(), ok: med.status === 0 };

// --- refusal flood: 2000 spawns hitting denied rt.kv -> exit 7 ----------------------
const refJs = sh('refuse.js', "rt.kv.get('x')\n");
const REF = 2000; t = now(); let refused = 0, other = {};
for (let i = 0; i < REF; i++) {
	const r = spawnSync(JSRT, ['run', refJs], { timeout: 30000 });
	if (r.status === 7) refused++; else other[r.status] = (other[r.status] || 0) + 1;
}
out.refusal = { spawns: REF, refused, other_exits: other, seconds: +((now() - t) / 1000).toFixed(1), ok: refused === REF };

// --- supervise churn: crash-loop workers, each bounded + reported -------------------
const crashJs = sh('crash.js', "throw new Error('worker died')\n");
const SUP = 100; t = now(); let done = 0;
for (let i = 0; i < SUP; i++) {
	const r = spawnSync(JSRT, ['supervise', crashJs], { encoding: 'utf8', timeout: 30000 });
	if (r.status !== null && r.stdout.includes('"outcome"')) done++;
}
out.supervise = { runs: SUP, reported: done, seconds: +((now() - t) / 1000).toFixed(1), ok: done === SUP };

const file = join(ROOT, 'perf', 'results', `sustained-${out.date.replace(/[:.]/g, '')}.json`);
writeFileSync(file, JSON.stringify(out, null, 2));
console.log(JSON.stringify(out, null, 1), '->', file);
