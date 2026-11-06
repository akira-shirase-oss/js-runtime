// M7.2a — the op boundary at scale: 1,000,000 rt.now() calls.
// Same shape as op-boundary.js (the empty-loop baseline is subtracted)
// at 100x the count — loop drift and GC pauses are part of the number.
const N = 1000000;

let t0 = rt.now();
let acc = 0;
for (let i = 0; i < N; i++) acc += i;
const loop_ns = ((rt.now() - t0) * 1e6) / N;

t0 = rt.now();
for (let i = 0; i < N; i++) rt.now();
const now_ns = ((rt.now() - t0) * 1e6) / N;

rt.print(JSON.stringify({
	n: N,
	loop_ns_per_call: loop_ns,
	now_ns_per_call: now_ns,
	now_boundary_ns_per_call: now_ns - loop_ns,
	calls_per_sec: Math.round(1e9 / now_ns),
	acc_sink: acc,
}) + "\n");
