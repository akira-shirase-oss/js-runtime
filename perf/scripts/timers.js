// Timer throughput: sequential sleep(0) measures scheduling overhead per
// timer; sequential sleep(1) measures drift against the requested delay.
const N0 = 500;
let t0 = rt.now();
for (let i = 0; i < N0; i++) await rt.sleep(0);
const zero_total = rt.now() - t0;

const N1 = 100;
t0 = rt.now();
let drift = 0;
for (let i = 0; i < N1; i++) {
	const s = rt.now();
	await rt.sleep(1);
	drift += rt.now() - s - 1;
}
const one_total = rt.now() - t0;

rt.print(JSON.stringify({
	sleep0_count: N0,
	sleep0_ms_per_call: zero_total / N0,
	sleep1_count: N1,
	sleep1_ms_per_call: one_total / N1,
	sleep1_avg_drift_ms: drift / N1,
}) + "\n");
