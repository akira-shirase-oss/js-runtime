// M8.x supervise churn — the worker each respawn runs: a health server
// on a fixed port so an orphan would hold the port and break the next
// respawn (the drill's proof, at scale). Port literal lives in my band.
const server = rt.serve({ host: "127.0.0.1", port: 47412 }, async () => {
	return { status: 200, body: "ok" };
});

await server.done;
