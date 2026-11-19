// M8.x media sustained — N streamed puts from a read-granted dir into a
// declared store root, dedup exercised every 10th op, then stat on every
// distinct name. __UNITS__, __SRC__, __FILES__ come from perf/load.py.
const N = __UNITS__;
const SRC = "__SRC__";
const FILES = __FILES__;

const t0 = rt.now();
let puts = 0;
let dedup_puts = 0;
let put_bytes = 0;
const names = new Set();
for (let i = 0; i < N; i++) {
	const src = SRC + "/src-" + (i % FILES) + ".bin";
	const r = rt.media.put(src);
	puts++;
	put_bytes += r.bytes;
	names.add(r.name);
	if (i % 10 === 0) {
		const again = rt.media.put(src);
		dedup_puts++;
		if (again.name !== r.name) throw new Error("dedup name moved");
	}
}
let stats = 0;
let stat_ok = 0;
for (const name of names) {
	const s = rt.media.stat(name);
	stats++;
	if (s && s.name === name && s.bytes > 0) stat_ok++;
}

rt.print(JSON.stringify({
	puts: N,
	put_wall_ms: Math.round(rt.now() - t0),
	dedup_puts,
	put_bytes,
	distinct_names: names.size,
	stats,
	stat_ok,
}) + "\n");
