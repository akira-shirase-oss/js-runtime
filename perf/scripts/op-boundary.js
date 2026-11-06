// Op-boundary cost: 10,000 calls each of a scalar-returning op (rt.now)
// and a serialization-returning op (rt.hostInfo). Prints one JSON line.
// The empty-loop baseline is subtracted so the numbers isolate the
// boundary crossing + (for hostInfo) serde serialization, not JS looping.
const N = 10000;

let t0 = rt.now();
let acc = 0;
for (let i = 0; i < N; i++) acc += i;
const loop_ns = ((rt.now() - t0) * 1e6) / N;

t0 = rt.now();
for (let i = 0; i < N; i++) rt.now();
const now_ns = ((rt.now() - t0) * 1e6) / N;

t0 = rt.now();
for (let i = 0; i < N; i++) rt.hostInfo();
const hostinfo_ns = ((rt.now() - t0) * 1e6) / N;

rt.print(JSON.stringify({
	n: N,
	loop_ns_per_call: loop_ns,
	now_ns_per_call: now_ns,
	now_boundary_ns_per_call: now_ns - loop_ns,
	hostinfo_ns_per_call: hostinfo_ns,
	hostinfo_boundary_ns_per_call: hostinfo_ns - loop_ns,
	acc_sink: acc,
}) + "\n");
