// M8.x refusal flood — N ungated op calls, each must refuse as a named
// CapabilityError a script can match, fast and clean. __UNITS__ from
// perf/load.py (1,000,000 at full scale).
const N = __UNITS__;

const t0 = rt.now();
let refused = 0;
let named = 0;
let other = 0;
const by_name = {};
const sink = [];
for (let i = 0; i < N; i++) {
	try {
		switch (i % 5) {
			case 0:
				rt.kv.get("k");
				break;
			case 1:
				rt.kv.put("k", "v");
				break;
			case 2:
				await rt.fetch("https://example.com/");
				break;
			case 3:
				rt.media.put("/tmp/nonexistent");
				break;
			default:
				await rt.readText("/etc/passwd");
		}
		other++;
	} catch (e) {
		refused++;
		const name = (e && e.name) || typeof e;
		by_name[name] = (by_name[name] || 0) + 1;
		if (name === "CapabilityError") named++;
	}
}

rt.print(JSON.stringify({
	attempts: N,
	refused,
	capability_named: named,
	not_refused: other,
	by_name,
	wall_ms: Math.round(rt.now() - t0),
	sink_len: sink.length,
}) + "\n");
