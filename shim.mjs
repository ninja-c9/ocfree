// ocfree transport shim.
//
// Upstream keys the free lane on the TLS ClientHello, so every request has to
// leave from Bun/BoringSSL. It also accounts quota per egress IP and per
// session id, so a conversation pins one (session, proxy) pair and keeps it
// until upstream answers 429 -- only then are both rotated, together.
//
// Protocol with the Rust gateway (POST /):
//   { url, method, headers, body, conv }
// `conv` is an opaque conversation key. Without it the shim sends no session
// headers (used for GET /models). The response is upstream's response, body
// streamed through unbuffered, plus `x-ocfree-session` / `x-ocfree-proxy`.

import { serve } from "bun";

const PORT = Number(process.env.OCFREE_SHIM_PORT || 8941);
const TOKEN = process.env.OCFREE_SHIM_TOKEN || "";
const RETRIES = Number(process.env.OCFREE_RETRIES || 3);
const BACKOFF_MS = Number(process.env.OCFREE_BACKOFF_MS || 500);
const CONV_TTL_MS = Number(process.env.OCFREE_CONV_TTL_MS || 30 * 60 * 1000);
const MAX_CONVS = Number(process.env.OCFREE_MAX_CONVS || 5000);
const PROXIES = String(process.env.OCFREE_PROXIES || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);

const SESSION_HEADERS = ["x-opencode-session", "x-opencode-session-id", "x-session-affinity", "x-session-id"];
// Hop-by-hop and encoding headers describe the upstream connection, not the
// bytes Bun hands us after decompression; forwarding them corrupts the body.
const DROP_HEADERS = new Set([
  "content-encoding",
  "content-length",
  "transfer-encoding",
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "upgrade",
]);

// Same derivation as the CLI's Identifier.descending(): 12 hex of
// ~(ms * 0x1000 + counter) & (2^48 - 1), then 14 base62 characters. The gate
// decodes the prefix, so the timestamp must be the real current time and the
// counter must stay small.
const A = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
let lastMs = 0;
let counter = 0;

function newSession() {
  const now = Date.now();
  if (now !== lastMs) {
    lastMs = now;
    counter = 0;
  }
  counter = (counter % 16) + 1;
  const hex = (~(BigInt(now) * 4096n + BigInt(counter)) & ((1n << 48n) - 1n))
    .toString(16)
    .padStart(12, "0");
  const bytes = crypto.getRandomValues(new Uint8Array(14));
  let random = "";
  for (const b of bytes) random += A[b % 62];
  return "ses_" + hex + random;
}

// conv key -> { session, proxy, lastUsed }. New conversations are spread over
// the proxy pool; an existing one never moves off its pair until a 429.
const conversations = new Map();
let nextProxy = 0;

function pin(conv) {
  let entry = conversations.get(conv);
  if (!entry) {
    entry = { session: newSession(), proxy: PROXIES.length ? nextProxy++ % PROXIES.length : 0, lastUsed: Date.now() };
    if (conversations.size >= MAX_CONVS) {
      conversations.delete(conversations.keys().next().value);
    }
    conversations.set(conv, entry);
  }
  entry.lastUsed = Date.now();
  return entry;
}

function proxyOf(entry) {
  return PROXIES.length ? PROXIES[entry.proxy % PROXIES.length] : undefined;
}

setInterval(() => {
  const now = Date.now();
  for (const [key, entry] of conversations) {
    if (now - entry.lastUsed > CONV_TTL_MS) conversations.delete(key);
  }
}, Math.min(CONV_TTL_MS, 60_000)).unref?.();

// Never print proxy credentials into the log.
function redact(url) {
  try {
    const parsed = new URL(url);
    if (parsed.username || parsed.password) {
      parsed.username = "***";
      parsed.password = "";
    }
    return parsed.toString();
  } catch {
    return "proxy";
  }
}

function passthroughHeaders(headers) {
  const out = new Headers();
  headers.forEach((value, name) => {
    if (!DROP_HEADERS.has(name.toLowerCase())) out.set(name, value);
  });
  return out;
}

serve({
  port: PORT,
  hostname: "127.0.0.1",
  idleTimeout: 0,
  async fetch(req) {
    const path = new URL(req.url).pathname;
    if (req.headers.get("x-ocfree-token") !== TOKEN) return new Response("forbidden", { status: 403 });
    if (req.method === "GET" && path === "/health") return new Response("ok");
    if (req.method !== "POST") return new Response("not found", { status: 404 });

    let p;
    try {
      p = await req.json();
    } catch {
      return Response.json({ error: "bad json" }, { status: 400 });
    }

    const conv = typeof p.conv === "string" && p.conv ? p.conv : "";
    const entry = conv ? pin(conv) : null;
    let status = 0;
    let lastHeaders = null;
    let lastBody = "";

    for (let attempt = 0; attempt <= RETRIES; attempt++) {
      const proxy = entry ? proxyOf(entry) : undefined;
      const headers = { ...(p.headers || {}) };
      if (entry) for (const name of SESSION_HEADERS) headers[name] = entry.session;

      let res = null;
      try {
        res = await fetch(p.url, {
          method: p.method || "GET",
          headers,
          body: p.method === "GET" ? undefined : p.body,
          proxy,
        });
      } catch (e) {
        status = 0;
        lastHeaders = null;
        lastBody = "";
        console.error(
          `${new Date().toISOString()} ${p.method || "POST"} ${p.url} via ${proxy ? redact(proxy) : "env/direct"} -> transport error: ${e?.message || e} (attempt ${attempt + 1})`,
        );
      }

      if (res) {
        status = res.status;
        console.error(
          `${new Date().toISOString()} ${p.method || "POST"} ${p.url} via ${proxy ? redact(proxy) : "env/direct"} -> ${status} (attempt ${attempt + 1})`,
        );
        if (status !== 429 && status < 500) {
          const headersOut = passthroughHeaders(res.headers);
          if (entry) headersOut.set("x-ocfree-session", entry.session);
          return new Response(res.body, { status, headers: headersOut });
        }
        lastHeaders = res.headers;
        try {
          lastBody = await res.text();
        } catch {
          lastBody = "";
        }
      }

      // Only a 429 (or a dead connection / 5xx) moves the conversation: the
      // next attempt, and every later request, uses a new session AND the next
      // proxy. A successful conversation never leaves its pair.
      if (entry) {
        entry.session = newSession();
        if (PROXIES.length) entry.proxy = (entry.proxy + 1) % PROXIES.length;
      }
      if (attempt < RETRIES) {
        await new Promise((r) => setTimeout(r, Math.round(BACKOFF_MS * 2 ** attempt)));
      }
    }

    const headersOut = lastHeaders ? passthroughHeaders(lastHeaders) : new Headers();
    if (entry) headersOut.set("x-ocfree-session", entry.session);
    if (status === 0) {
      lastBody = JSON.stringify({ error: { type: "transport", message: "upstream unavailable" } });
      status = 502;
    }
    return new Response(lastBody, { status, headers: headersOut });
  },
});

// Exit with the gateway that spawned us, so a crashed gateway leaves no orphan.
const PARENT = process.ppid;
if (PARENT && PARENT !== 1) {
  setInterval(() => {
    try {
      process.kill(PARENT, 0);
    } catch {
      process.exit(0);
    }
  }, 5000).unref?.();
}

console.error(`shim on 127.0.0.1:${PORT} proxies=${PROXIES.length} retries=${RETRIES} ttl=${CONV_TTL_MS}ms`);
