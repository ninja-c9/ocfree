import { serve } from "bun";

const port = parseInt(process.env["OCFREE_SHIM_PORT"]) || 8941;
const token = process.env["OCFREE_SHIM_TOKEN"] || "";
const host = "127.0.0.1";

const server = Bun.serve({
  host,
  port,
  async fetch(req) {
    const url = new URL(req.url);
    const method = req.method;

    // GET /health -> 200 ok
    if (url.pathname === "/health") {
      return new Response("ok", { status: 200 });
    }

    // Check auth token for non-health routes
    const authToken = req.headers.get("x-ocfree-token");
    if (authToken !== token) {
      return new Response("Forbidden", { status: 403 });
    }

    // POST / - forward request
    if (method === "POST" && url.pathname === "/") {
      try {
        const body = await req.json();
        const { url: targetUrl, method: targetMethod, headers: targetHeaders, body: targetBody } = body;

        const fetchInit = {
          method: targetMethod || "GET",
          headers: targetHeaders || {},
        };

        // Add body if present and not GET
        if (targetBody && targetMethod !== "GET") {
          fetchInit.body = targetBody;
        }

        const res = await fetch(targetUrl, fetchInit);
        const text = await res.text();

        const headersArray = [];
        for (const [key, value] of res.headers) {
          headersArray.push([key, value]);
        }

        const responseJson = {
          status: res.status,
          headers: headersArray,
          body: text,
        };

        // Always return 200 with the JSON
        return new Response(JSON.stringify(responseJson), { status: 200, headers: { "Content-Type": "application/json" } });
      } catch (e) {
        const errorJson = {
          status: -1,
          headers: [],
          body: e instanceof Error ? e.message : String(e),
        };
        return new Response(JSON.stringify(errorJson), { status: 200, headers: { "Content-Type": "application/json" } });
      }
    }

    // Default 404
    return new Response("Not Found", { status: 404 });
  },
});

console.log(`Bun shim listening on http://${host}:${port}`);
