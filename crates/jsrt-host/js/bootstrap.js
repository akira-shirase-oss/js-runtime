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
};
