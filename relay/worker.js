// herdr-relay worker: a thin outbound proxy between devices and one bot token.
//
// Devices never hold the bot token. They speak the standard Bot API against this
// Worker with RELAY_KEY in the token position and the device name in a path
// prefix — so any off-the-shelf SDK connects with just base_url + token.
//
//   POST /d/<device>/bot<RELAY_KEY>/<method>   forwarded to Telegram (whitelist)
//
// The flow is strictly one-way: devices push messages out, nothing comes back.
// deleteWebhook is whitelisted (and NOT answered locally) so a one-time
// teardown of any webhook left on the bot is possible — otherwise Telegram
// would keep pushing updates at a 404.
//
// Bindings:
//   DB         D1 database — the api_calls audit table (schema in migrations.sql)
//   BOT_TOKEN  secret — the bot token, never leaves the Worker
//   RELAY_KEY  secret — shared key devices authenticate with

const API = "https://api.telegram.org";
const DAY = 86400;

// Everything a device may ask Telegram to do. Deliberately closed: the plugin
// only ever sends messages (plus getMe on initialize).
const ALLOWED_METHODS = new Set([
  "getMe",
  "sendMessage",
  "sendRichMessage",
  "deleteWebhook",
]);

const json = (obj, status = 200) =>
  new Response(JSON.stringify(obj), {
    status,
    headers: { "content-type": "application/json" },
  });

// D1 throws on any storage failure, above all when the daily row-read budget is
// exhausted (code 7500). Unhandled, that throw escapes as a Worker error and the
// edge answers `error code: 1101` in plain text — 1101 reads as "could not reach
// the upstream host", which sent a whole debugging session chasing api.telegram.org
// DNS while the real cause was the quota. That is why the audit write below is
// wrapped: an audit failure must answer something honest, and it must not fail a
// send Telegram already accepted (a 503 there would make the device retry and
// post the message twice).
async function audit(env, device, method, params, body, started) {
  try {
    await env.DB.prepare(
      "INSERT INTO api_calls (device, method, params, ok, error_code, description, duration_ms, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
    )
      .bind(
        device,
        method,
        JSON.stringify(params ?? {}),
        body?.ok ? 1 : 0,
        body?.error_code ?? null,
        body?.description ?? null,
        Date.now() - started,
        Math.floor(started / 1000)
      )
      .run();
  } catch (e) {
    // the audit is a side channel: log and move on, the Telegram answer stands
    console.error(`audit write failed: ${e?.code || "?"} ${e?.message || e}`);
  }
}

async function telegram(env, method, params) {
  const resp = await fetch(`${API}/bot${env.BOT_TOKEN}/${method}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(params || {}),
  });
  // Telegram reports its 4xx (429 flood control above all) as a JSON error body
  // with the HTTP status set, so the status must travel back with the body. Dropped,
  // the SDK sees a 200 whose payload has no "result" and reads a 429 as an untyped
  // parse failure instead of a RetryAfter — the client then hammers Telegram at
  // 1s intervals and never leaves the flood-control window.
  const text = await resp.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    // A non-JSON reply (an error page, a truncated body) is still an answer worth
    // surfacing: echoing the status keeps the caller's error path honest.
    body = { ok: false, error_code: resp.status, description: `non-JSON reply from Telegram: ${text.slice(0, 200)}` };
  }
  return { status: resp.status, body };
}

// Devices may post either a JSON body or urlencoded form data (urlencoded
// clients often carry each parameter individually JSON-encoded). Form values
// that are obviously structured (they start with { or [) are decoded back
// into objects — bare strings stay strings, which Telegram's API coerces for
// number-typed params the same way it does for every form-posting client.
async function readParams(request) {
  const type = (request.headers.get("content-type") || "").split(";")[0].trim();
  if (type === "application/x-www-form-urlencoded") {
    const form = await request.formData();
    const params = {};
    for (const [name, value] of form.entries()) {
      const v = String(value);
      params[name] = v.startsWith("{") || v.startsWith("[") ? tryParse(v) : v;
    }
    return params;
  }
  try {
    return await request.json();
  } catch {
    // an empty body means default parameters
    return {};
  }
}

function tryParse(text) {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);

    const call = pathname.match(/^\/d\/([^/]+)\/bot([^/]+)\/(\w+)$/);
    if (request.method === "POST" && call) {
      const [, rawDevice, key, method] = call;
      if (key !== env.RELAY_KEY) {
        // 401 is what a real Bot API server answers for a bad token, and what
        // SDKs map to their invalid-token error
        return json({ ok: false, error_code: 401, description: "Unauthorized" }, 401);
      }
      // Device names may be any Unicode letters/digits — CJK included — so the URL
      // carries them percent-encoded. Decode before validating; a malformed escape
      // or anything beyond letters/digits/_/- (up to 64 chars) is not a device.
      let device = "";
      try {
        device = decodeURIComponent(rawDevice);
      } catch {
        // fall through with an invalid name
      }
      if (!/^[\p{L}\p{N}_-]{1,64}$/u.test(device)) {
        return json({ ok: false, error_code: 404, description: "invalid device name" }, 404);
      }
      if (!ALLOWED_METHODS.has(method)) {
        return json({ ok: false, error_code: 400, description: `method ${method} is not allowed` }, 400);
      }
      const params = await readParams(request);
      const started = Date.now();
      const { status, body } = await telegram(env, method, params);
      await audit(env, device, method, params, body, started);
      return json(body, status);
    }

    return json({ ok: false, description: "not found" }, 404);
  },

  // Age out the audit log; runs daily via the cron trigger.
  async scheduled(_controller, env) {
    const now = Math.floor(Date.now() / 1000);
    try {
      await env.DB.prepare("DELETE FROM api_calls WHERE created_at < ?1").bind(now - 30 * DAY).run();
    } catch (e) {
      // A delete still scans, so it spends row-read budget: the sweep fails when the
      // budget is gone and would otherwise show up as an unhandled cron error. The
      // stale rows survive and are retried on the next run.
      console.error(`retention sweep failed: ${e?.code || "?"} ${e?.message || e}`);
    }
  },
};
