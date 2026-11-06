// M7.2b — the load target for rt.serve's million-request run.
// The handler is deliberately trivial: a fixed 200 + short body, so the
// measurement is the serve path (wire -> dispatch -> respond), not app
// work. KV and friends are measured separately.
const server = rt.serve({ host: "127.0.0.1", port: 9410 }, async () => {
	return { status: 200, body: "ok" };
});

await server.done;
