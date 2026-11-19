// M8.x kv sustained — N mixed ops on a granted store, integrity checked
// per op, then quota pressure until the grant refuses. __UNITS__ is
// substituted by perf/load.py (1,000,000 at full scale).
const N = __UNITS__;
const W = 1024; // working set: keys cycle so every op class repeats

const t0 = rt.now();
const expected = new Map();
let puts = 0;
let gets = 0;
let gets_hit = 0;
let deletes = 0;
let lists = 0;
let integrity_fail = 0;

for (let i = 0; i < N; i++) {
	// Ops are grouped in blocks of 8: 3 puts, 3 gets, 1 delete, 1 list.
	// Reads and the delete target the block's first written key so the
	// read path actually hits (a get that always misses proves nothing).
	const k = "k" + ((i - (i % 8)) % W);
	switch (i % 8) {
		case 0:
		case 1:
		case 2: {
			const v = "v" + i;
			rt.kv.put("k" + (i % W), v);
			expected.set("k" + (i % W), v);
			puts++;
			break;
		}
		case 3:
		case 4:
		case 5: {
			const got = rt.kv.get(k);
			gets++;
			if (got !== null) {
				gets_hit++;
				if (got !== expected.get(k)) integrity_fail++;
			}
			break;
		}
		case 6: {
			rt.kv.delete(k);
			expected.delete(k);
			deletes++;
			break;
		}
		default: {
			rt.kv.list("k" + (i % 64), 100);
			lists++;
		}
	}
}
const mixed_ms = (rt.now() - t0);

// Quota pressure: writes refuse at the declared size and the store keeps
// answering. Every refusal must classify as CapabilityError.
const t1 = rt.now();
const value = "x".repeat(65536);
let refusals = 0;
let quota_puts = 0;
let named = 0;
for (let i = 0; i < 100000; i++) {
	try {
		rt.kv.put("big-" + i, value);
		quota_puts++;
	} catch (e) {
		refusals++;
		if (e && e.name === "CapabilityError") named++;
	}
	if (refusals >= 200) break;
}
// The store still answers after the flood.
rt.kv.put("post-quota", "ok");
const post_ok = rt.kv.get("post-quota") === "ok";
const quota_ms = (rt.now() - t1);

rt.print(JSON.stringify({
	ops: N,
	mixed_wall_ms: Math.round(mixed_ms),
	puts,
	gets,
	gets_hit,
	deletes,
	lists,
	integrity_fail,
	quota_puts,
	quota_refusals: refusals,
	refusals_named_capability: named,
	post_quota_ok: post_ok,
	quota_wall_ms: Math.round(quota_ms),
}) + "\n");
