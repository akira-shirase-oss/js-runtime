// The webhook app — a real program on jsrt, using rt.serve + rt.kv.
//
// POST /hooks/:name  stores the body under the X-Delivery-Id header's
//                    dedup key; a repeat of the same id answers
//                    "duplicate" without a second write.
// GET  /hooks/:name  lists the stored delivery keys, newest last.
// GET  /health       the drill's liveness check.
//
// v1.

const server = rt.serve({ host: "127.0.0.1", port: 9400 }, async (req) => {
	const path = req.path.split("?")[0];
	if (path === "/health") {
		return { status: 200, body: JSON.stringify({ ok: true, v: 1 }) };
	}
	if (!path.startsWith("/hooks/") || path.length <= 7) {
		return { status: 404, body: "not found" };
	}
	const name = path.slice(7);
	if (req.method === "POST") {
		const id =
			(req.headers.find(([k]) => k.toLowerCase() === "x-delivery-id") || [])[1] ||
			String(rt.now());
		const key = `hook/${name}/${id}`;
		if (rt.kv.get(key) !== null) {
			return { status: 200, body: "duplicate" };
		}
		rt.kv.put(key, req.body);
		return { status: 201, body: `stored ${key}` };
	}
	if (req.method === "GET") {
		const keys = rt.kv.list(`hook/${name}/`).map((e) => e.key);
		return { status: 200, body: JSON.stringify(keys) };
	}
	return { status: 405, body: "method not allowed" };
});

rt.print("webhook v1 serving on 127.0.0.1:9400\n");
await server.done;
