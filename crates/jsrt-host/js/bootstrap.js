// The script-facing namespace. Everything a script can do lives under `rt.*`
// -- there is no ambient `Deno`, no ungated global. Each entry is one op; each
// op checks the capability manifest before acting.
//
// This file is the API contract: keep it small, keep names honest, and never
// expose an op here that the docs don't describe.
const ops = Deno.core.ops;

// Error classes for op rejections. The class strings in ops.rs
// (JsErrorBox::new) mean nothing to V8 until a constructor is registered here
// -- registerErrorClass is what lets an op rejection become the right kind of
// object on the JS side, and what lets classify() map it back to an exit code.
class CapabilityError extends Error {
	constructor(msg) { super(msg); this.name = "CapabilityError"; }
}
class HostError extends Error {
	constructor(msg) { super(msg); this.name = "HostError"; }
}
Deno.core.registerErrorClass("CapabilityError", CapabilityError);
Deno.core.registerErrorClass("HostError", HostError);

// Timers: Node-style semantics over deno_core's built-in timer machinery.
// Ref-counted timers hold the event loop open until they fire (the watchdog
// is the bound); unref'd timers don't. IDs returned to scripts are ours --
// the map also gives teardown a place to enumerate pending work later.
const timers = new Map();
function track(timer) {
	const id = timer._timerId;
	timers.set(id, timer);
	return id;
}
function untrack(id) {
	const timer = timers.get(id);
	if (timer !== undefined) {
		Deno.core.cancelTimer(timer);
		timers.delete(id);
	}
}
function schedule(cb, ms, args, repeat) {
	const holder = {};
	const wrapped = (...a) => {
		if (!repeat) timers.delete(holder.id);
		cb(...a);
	};
	holder.id = track(Deno.core.createTimer(wrapped, ms, args, repeat, true, false));
	return holder.id;
}

globalThis.rt = {
	/** Write to stdout with no added newline (deno_core's builtin op_print). */
	print: (msg) => Deno.core.print(String(msg), false),
	/** {runtime, version, engine} -- who's hosting this script. */
	hostInfo: () => ops.op_host_info(),
	/** Wall clock in milliseconds, for measurements. */
	now: () => ops.op_now(),
	/** Sleep for `ms` -- the async timer primitive. */
	sleep: (ms) => ops.op_sleep(ms),
	/** Read a text file inside a declared read path. Throws CapabilityError outside. */
	readText: (path) => ops.op_read_text(path),
	/** fetch a declared net destination. Throws CapabilityError outside. */
	fetch: (url) => ops.op_fetch(url),
	/** One-shot timer: cb(...args) after ms. Returns an id for clearTimeout. */
	setTimeout: (cb, ms, ...args) => schedule(cb, ms, args, false),
	/** Repeating timer: cb(...args) every ms. Returns an id for clearInterval. */
	setInterval: (cb, ms, ...args) => schedule(cb, ms, args, true),
	/** Cancel a pending setTimeout/setInterval. Already-fired ids are no-ops. */
	clearTimeout: (id) => untrack(id),
	clearInterval: (id) => untrack(id),
	/** Let the event loop exit without waiting for this timer. */
	unrefTimer: (id) => {
		const t = timers.get(id);
		if (t !== undefined) Deno.core.unrefTimer(t);
	},
	/** Undo unrefTimer -- the timer holds the loop open again. */
	refTimer: (id) => {
		const t = timers.get(id);
		if (t !== undefined) Deno.core.refTimer(t);
	},
	/** Timer ids this script still owns (fired one-shots remove themselves). */
	pendingTimers: () => timers.size,
	/**
	 * The embedded store, gated by the manifest's "kv" line. Synchronous on
	 * purpose: SQLite on a local file answers in microseconds, and an honest
	 * block beats a fake async. No grant -> every call throws CapabilityError.
	 */
	kv: {
		/** The value, or null. */
		get: (key) => ops.op_kv_get(String(key)),
		/** Upsert; throws CapabilityError past the declared quota. */
		put: (key, value) => ops.op_kv_put(String(key), String(value)),
		/** Whether anything was removed. */
		delete: (key) => ops.op_kv_delete(String(key)),
		/** [{key, value}] under an exact prefix, key-sorted. */
		list: (prefix = "", limit = 1000) => ops.op_kv_list(String(prefix), limit),
	},
	/**
	 * Content-addressed media store, gated by the manifest's "media" line.
	 * put streams a read-granted file into the store under its sha256 name --
	 * never a whole-file buffer, and a mid-upload failure cleans its partial.
	 */
	media: {
		/** {name, bytes, sha256, state:"stored"}; name is the sha256 hex. */
		put: (srcPath) => ops.op_media_put(String(srcPath)),
		/** {name, bytes} for a stored name, else null. */
		stat: (name) => ops.op_media_stat(String(name)),
	},
};
