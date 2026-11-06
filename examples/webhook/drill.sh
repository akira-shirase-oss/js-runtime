#!/usr/bin/env bash
# The ship-gate drill, verbatim: deploy -> update -> break -> inspect ->
# rollback, the app running under `jsrt supervise` the whole time.
# Every step's evidence lands in drill.log; the report JSONL is kept too.
set -u
cd "$(dirname "$0")"

JSRT="${JSRT:-$(cd ../.. && pwd)/target/debug/jsrt}"
BASE="http://127.0.0.1:9400"
LOG=drill.log
: > "$LOG"
mkdir -p data

note() { echo "=== $1" | tee -a "$LOG"; }
say() { echo "    $1" | tee -a "$LOG"; }

# ---- deploy (v1) ---------------------------------------------------------
note "DEPLOY v1 — jsrt supervise, manifest grant, health check"
cp app.js current.js
$JSRT supervise current.js \
	--manifest jsrt.manifest.json --report data/runs.jsonl --wall-ms 30000 &
SUP=$!
for i in $(seq 1 50); do
	curl -sf "$BASE/health" >/dev/null 2>&1 && break
	sleep 0.1
done
say "health: $(curl -s "$BASE/health")"
say "post d1: $(curl -s -X POST "$BASE/hooks/github" -H 'X-Delivery-Id: d1' -d '{"a":1}')"
say "post d1 again (dedup): $(curl -s -X POST "$BASE/hooks/github" -H 'X-Delivery-Id: d1' -d '{"a":1}')"
say "post d2: $(curl -s -X POST "$BASE/hooks/github" -H 'X-Delivery-Id: d2' -d '{"b":2}')"
say "list /hooks/github: $(curl -s "$BASE/hooks/github")"

# ---- update (v1 -> v2) ---------------------------------------------------
note "UPDATE v2 — swap current.js, respawn, verify new route + data survived"
kill $SUP 2>/dev/null; wait $SUP 2>/dev/null
sed 's/v: 1/v: 2/; s/webhook v1/webhook v2/' current.js > current.js.tmp && mv current.js.tmp current.js
$JSRT supervise current.js \
	--manifest jsrt.manifest.json --report data/runs.jsonl --wall-ms 30000 &
SUP=$!
for i in $(seq 1 50); do
	curl -sf "$BASE/health" >/dev/null 2>&1 && break
	sleep 0.1
done
say "health after update: $(curl -s "$BASE/health")"
say "list survived update: $(curl -s "$BASE/hooks/github")"

# ---- break (v2 -> broken) ------------------------------------------------
note "BREAK — handler throws on every route; health must still be answerable by the wire"
kill $SUP 2>/dev/null; wait $SUP 2>/dev/null
cat > current.js <<'EOF'
const server = rt.serve({host: "127.0.0.1", port: 9400}, async (req) => {
	throw new Error("v-broken: every request throws");
});
await server.done;
EOF
$JSRT supervise current.js \
	--manifest jsrt.manifest.json --report data/runs.jsonl --wall-ms 30000 &
SUP=$!
for i in $(seq 1 50); do
	nc -z 127.0.0.1 9400 2>/dev/null && break
	sleep 0.1
done
say "health under broken build: $(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")"
say "post under broken build: $(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/hooks/x" -d '{}')"

# ---- inspect -------------------------------------------------------------
note "INSPECT — the report knows it ran, the wire knows it served 500s"
say "report records so far: $(wc -l < data/runs.jsonl | tr -d ' ')"
tail -2 data/runs.jsonl | while read -r line; do say "  $line"; done

# ---- rollback (broken -> v1) ----------------------------------------------
note "ROLLBACK — restore v1 file, respawn, verify health + prior data"
kill $SUP 2>/dev/null; wait $SUP 2>/dev/null
cp app.js current.js
$JSRT supervise current.js \
	--manifest jsrt.manifest.json --report data/runs.jsonl --wall-ms 30000 &
SUP=$!
for i in $(seq 1 50); do
	curl -sf "$BASE/health" >/dev/null 2>&1 && break
	sleep 0.1
done
say "health after rollback: $(curl -s "$BASE/health")"
say "data after rollback: $(curl -s "$BASE/hooks/github")"

# ---- shutdown --------------------------------------------------------------
kill $SUP 2>/dev/null; wait $SUP 2>/dev/null
say "final report records: $(wc -l < data/runs.jsonl | tr -d ' ')"
note "DRILL COMPLETE"
