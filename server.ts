import { Hono } from "hono";
import type { Context } from "hono";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// When compiled with `bun build --compile`, import.meta.url points into the
// embedded bundle, so we resolve paths next to the executable instead.
const compiled = import.meta.url.includes("$bunfs") || import.meta.url.includes("~BUN");
const ROOT = compiled ? dirname(process.execPath) : dirname(fileURLToPath(import.meta.url));

const PORT = Number(process.env.PORT ?? 8976);
const REDIRECT_URI = `http://127.0.0.1:${PORT}/callback`;
const DATA_FILE = join(ROOT, "data.json");

type Store = {
  client_id?: string;
  refresh_token?: string;
  access_token?: string;
  expires_at?: number;
};

let store: Store = existsSync(DATA_FILE)
  ? JSON.parse(readFileSync(DATA_FILE, "utf8"))
  : {};

function save() {
  writeFileSync(DATA_FILE, JSON.stringify(store, null, 2));
}

// state -> code_verifier, only kept for the login round-trip
const pending = new Map<string, string>();

function randomString(len: number) {
  const bytes = crypto.getRandomValues(new Uint8Array(len));
  return Array.from(bytes, (b) => "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"[b % 62]).join("");
}

async function sha256base64url(input: string) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(input));
  return btoa(String.fromCharCode(...new Uint8Array(digest)))
    .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function getAccessToken(): Promise<string | null> {
  if (!store.client_id || !store.refresh_token) return null;
  if (store.access_token && store.expires_at && Date.now() < store.expires_at - 30_000) {
    return store.access_token;
  }
  const res = await fetch("https://accounts.spotify.com/api/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: store.refresh_token,
      client_id: store.client_id,
    }),
  });
  if (!res.ok) {
    console.error("Token refresh failed:", res.status, await res.text());
    return null;
  }
  const data = await res.json();
  store.access_token = data.access_token;
  store.expires_at = Date.now() + data.expires_in * 1000;
  // Spotify rotates the refresh token on PKCE
  if (data.refresh_token) store.refresh_token = data.refresh_token;
  save();
  return store.access_token!;
}

const app = new Hono();

app.get("/api/status", (c: Context) =>
  c.json({
    configured: Boolean(store.client_id),
    authed: Boolean(store.refresh_token),
    redirect_uri: REDIRECT_URI,
  })
);

app.post("/api/client-id", async (c: Context) => {
  const { client_id } = await c.req.json();
  if (typeof client_id !== "string" || client_id.trim().length < 10) {
    return c.json({ error: "invalid_client_id" }, 400);
  }
  store.client_id = client_id.trim();
  save();
  return c.json({ ok: true });
});

app.get("/login", async (c: Context) => {
  if (!store.client_id) return c.text("Save a client ID first (config page).", 400);
  const verifier = randomString(64);
  const state = randomString(16);
  pending.set(state, verifier);
  const params = new URLSearchParams({
    response_type: "code",
    client_id: store.client_id,
    scope: "user-read-currently-playing user-read-playback-state",
    redirect_uri: REDIRECT_URI,
    state,
    code_challenge_method: "S256",
    code_challenge: await sha256base64url(verifier),
  });
  return c.redirect("https://accounts.spotify.com/authorize?" + params.toString());
});

app.get("/callback", async (c: Context) => {
  const code = c.req.query("code");
  const state = c.req.query("state") ?? "";
  const verifier = pending.get(state);
  pending.delete(state);
  if (!code || !verifier || !store.client_id) {
    return c.text("Callback without a valid code/state. Try again via /login.", 400);
  }
  const res = await fetch("https://accounts.spotify.com/api/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code,
      redirect_uri: REDIRECT_URI,
      client_id: store.client_id,
      code_verifier: verifier,
    }),
  });
  if (!res.ok) {
    return c.text("Token exchange failed: " + (await res.text()), 500);
  }
  const data = await res.json();
  store.access_token = data.access_token;
  store.refresh_token = data.refresh_token;
  store.expires_at = Date.now() + data.expires_in * 1000;
  save();
  return c.redirect("/?connected=1");
});

app.get("/api/now-playing", async (c: Context) => {
  const token = await getAccessToken();
  if (!token) return c.json({ playing: false, authed: false });

  const res = await fetch("https://api.spotify.com/v1/me/player/currently-playing", {
    headers: { Authorization: "Bearer " + token },
  });

  // 204 = nothing playing right now
  if (res.status === 204) return c.json({ playing: false, authed: true });
  if (!res.ok) return c.json({ playing: false, authed: true, error: res.status });

  const data = await res.json();
  const item = data.item;
  if (!item || data.currently_playing_type !== "track") {
    return c.json({ playing: false, authed: true });
  }
  const images = item.album?.images ?? [];
  return c.json({
    playing: data.is_playing === true,
    authed: true,
    id: item.id,
    title: item.name,
    artists: item.artists.map((a: { name: string }) => a.name).join(", "),
    cover: images[1]?.url ?? images[0]?.url ?? null,
    progress_ms: data.progress_ms ?? 0,
    duration_ms: item.duration_ms ?? 0,
  });
});

// Next track from the queue, feeds the "Up Next" banner
app.get("/api/next", async (c: Context) => {
  const token = await getAccessToken();
  if (!token) return c.json({});
  const res = await fetch("https://api.spotify.com/v1/me/player/queue", {
    headers: { Authorization: "Bearer " + token },
  });
  // 403 here almost always means: no Premium (the queue endpoint is Premium-only)
  if (!res.ok) return c.json({ error: res.status });
  const data = await res.json();
  const next = (data.queue ?? []).find((i: any) => i?.type === "track");
  if (!next) return c.json({});
  const images = next.album?.images ?? [];
  return c.json({
    title: next.name,
    artists: next.artists.map((a: { name: string }) => a.name).join(", "),
    cover: images[2]?.url ?? images[1]?.url ?? images[0]?.url ?? null,
  });
});

// Covers are proxied so the canvas color extraction doesn't run into CORS
app.get("/api/cover", async (c: Context) => {
  const url = c.req.query("url") ?? "";
  let host = "";
  try {
    host = new URL(url).hostname;
  } catch {
    return c.text("bad url", 400);
  }
  if (!host.endsWith(".scdn.co") && !host.endsWith(".spotifycdn.com")) {
    return c.text("host not allowed", 403);
  }
  const res = await fetch(url);
  if (!res.ok) return c.text("fetch failed", 502);
  return new Response(res.body, {
    headers: {
      "Content-Type": res.headers.get("Content-Type") ?? "image/jpeg",
      "Cache-Control": "public, max-age=86400",
    },
  });
});

const page = (name: string) => async (c: Context) =>
  c.html(await readFile(join(ROOT, "public", name), "utf8"));
app.get("/", page("config.html"));
app.get("/overlay", page("overlay.html"));

console.log(`Running at http://127.0.0.1:${PORT}`);
console.log(`Redirect URI for your Spotify app: ${REDIRECT_URI}`);

// Bun picks up the default export; on Node we start the adapter ourselves
if (!("Bun" in globalThis)) {
  const { serve } = await import("@hono/node-server");
  serve({ fetch: app.fetch, port: PORT });
}

export default { port: PORT, fetch: app.fetch };
