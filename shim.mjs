import { serve } from "bun";
const PORT = Number(process.env.OCFREE_SHIM_PORT || 8941);
const TOKEN = process.env.OCFREE_SHIM_TOKEN || "";
const HOST = "127.0.0.1";
const RETRIES = Number(process.env.OCFREE_RETRIES || 3);
const PROXIES = String(process.env.OCFREE_PROXIES || process.env.HTTPS_PROXY || "")
  .split(",").map(s => s.trim()).filter(Boolean);

const A = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
function newSession() {
  const t = Date.now();
  const hex = (~(BigInt(t) * 4096n + 1n) & ((1n << 48n) - 1n)).toString(16).padStart(12, "0");
  const b = crypto.getRandomValues(new Uint8Array(14));
  let r = "";
  for (const x of b) r += A[x % 62];
  return "ses_" + hex + r;
}

const SESSION_HEADERS = ["x-opencode-session", "x-opencode-session-id", "x-session-affinity", "x-session-id"];

async function once(targetUrl, targetMethod, targetHeaders, targetBody, proxy) {
  const init = { method: targetMethod || "GET", headers: targetHeaders || {} };
  if (targetBody && targetMethod !== "GET") init.body = targetBody;
  if (proxy) init.proxy = proxy;
  const res = await fetch(targetUrl, init);
  return { status: res.status, headers: [...res.headers], body: await res.text() };
}

serve({
  port: PORT,
  hostname: HOST,
  async fetch(req) {
    const path = new URL(req.url).pathname;
    if (req.method === "GET" && path === "/health") return new Response("ok");
    if (req.headers.get("x-ocfree-token") !== TOKEN) return new Response("forbidden", { status: 403 });
    if (req.method !== "POST") return new Response("not found", { status: 404 });

    let p;
    try { p = await req.json(); } catch { return Response.json({ error: "bad json" }, { status: 400 }); }

    let last = { status: 0, headers: [], body: "no attempt" };
    for (let attempt = 0; attempt <= RETRIES; attempt++) {
      const proxy = PROXIES.length ? PROXIES[attempt % PROXIES.length] : undefined;
      const headers = { ...(p.headers || {}) };
      if (attempt > 0) {
        const s = newSession();
        for (const k of SESSION_HEADERS) if (k in headers) headers[k] = s;
      }
      try {
        last = await once(p.url, p.method, headers, p.body, proxy);
      } catch (e) {
        last = { status: 0, headers: [], body: String(e && e.message || e) };
      }
      console.error(`${p.method || "POST"} ${p.url} via ${proxy || "direct"} -> ${last.status} (attempt ${attempt + 1})`);
      if (!(last.status === 429 || last.status === 0 || last.status >= 500)) break;
    }
    return Response.json(last);
  },
});
console.error(`shim on ${HOST}:${PORT} proxies=${PROXIES.length} retries=${RETRIES}`);
