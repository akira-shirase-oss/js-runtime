// M7.3 — the soak workload: timers + loopback fetch churn for one hour.
// What a leak looks like here: pending timers that never drain, retained
// request handles, RSS that only climbs. The script prints a JSON status
// line every STATUS_EVERY iterations so the driver can correlate the
// runtime's own retained-work view with the OS's RSS view.
//
// Work per iteration:
//   - 8 setTimeout(0) that fire and free themselves
//   - 1 setInterval that self-cancels after 3 ticks (exercises cancel path)
//   - every 4th: a fetch to the local serve target (response fully read)
const STATUS_EVERY = 60;
const TICKS = 3;
const FETCH_EVERY = 4;

let iters = 0;
let fetches = 0;
let fetchErrors = 0;
let intervalTick = 0;
let intervalFires = 0;

const iv = rt.setInterval(() => {
	intervalFires++;
	if (++intervalTick >= TICKS) {
		intervalTick = 0;
		rt.clearInterval(iv);
		// reschedule a fresh interval — the churn is the point
		startInterval();
	}
}, 1);

function startInterval() {
	const h = rt.setInterval(() => {
		intervalFires++;
		if (++intervalTick >= TICKS) {
			intervalTick = 0;
			rt.clearInterval(h);
			startInterval();
		}
	}, 1);
}

async function tick() {
	iters++;
	for (let i = 0; i < 8; i++) rt.setTimeout(() => {}, 0);
	if (iters % FETCH_EVERY === 0) {
		try {
			// rt.fetch returns {status, body, bytes, ...} — the body is
			// already the capped, materialized string.
			const r = await rt.fetch("http://127.0.0.1:9410/");
			if (r.status !== 200) fetchErrors++; else fetches++;
		} catch (e) {
			fetchErrors++;
		}
	}
	if (iters % STATUS_EVERY === 0) {
		rt.print(JSON.stringify({
			iters, fetches, fetchErrors, intervalFires,
			pending: rt.pendingTimers(),
		}) + "\n");
	}
	rt.setTimeout(tick, 0);
}

await tick();
