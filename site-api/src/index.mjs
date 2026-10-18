const enc = new TextEncoder();
const dec = new TextDecoder("utf-8", { fatal: true });
const ID_RE = /^[A-Z0-9][A-Z0-9_-]{2,31}$/;
const FIELD_RE = /^[\p{L}\p{N} ._+()/-]{1,48}$/u;
const OPTIONAL_RE = /^[\p{L}\p{N} ._+()/-]{0,100}$/u;
const MAX_LOG = 5 * 1024 * 1024;
function base64(bytes) {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}

const json = (value, status = 200, headers = {}) => new Response(JSON.stringify(value), {
  status, headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store", ...headers }
});
const b64url = bytes => {
  let raw = ""; for (const b of bytes) raw += String.fromCharCode(b);
  return btoa(raw).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
};
const unb64url = value => Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - value.length % 4) % 4)), c => c.charCodeAt(0));
const derLength = n => {
  if (n < 128) return Uint8Array.of(n);
  const out = []; while (n) { out.unshift(n & 255); n >>>= 8; }
  return Uint8Array.of(0x80 | out.length, ...out);
};
const der = (tag, bytes) => {
  const len = derLength(bytes.length), out = new Uint8Array(1 + len.length + bytes.length);
  out[0] = tag; out.set(len, 1); out.set(bytes, 1 + len.length); return out;
};
const pkcs8 = key => der(0x30, Uint8Array.from([
  0x02, 0x01, 0x00, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
  ...der(0x04, key)
]));
const siteBase = env => "https://" + env.SITE_ORIGIN.replace(/^https?:\/\//, "") + (env.SITE_PATH || "");
const apiBase = request => new URL(request.url).origin;

function cors(env, request) {
  const origin = request.headers.get("Origin");
  if (origin !== "https://" + env.SITE_ORIGIN.replace(/^https?:\/\//, "")) return {};
  return { "access-control-allow-origin": origin, "access-control-allow-methods": "GET, POST, OPTIONS",
    "access-control-allow-headers": "Authorization, Content-Type", "access-control-max-age": "600",
    "x-content-type-options": "nosniff", "referrer-policy": "no-referrer", vary: "Origin" };
}
async function keyFor(env) {
  return crypto.subtle.importKey("raw", enc.encode(env.SESSION_SECRET), { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
}
async function sign(env, value) {
  const payload = b64url(enc.encode(JSON.stringify(value)));
  return payload + "." + b64url(new Uint8Array(await crypto.subtle.sign("HMAC", await keyFor(env), enc.encode(payload))));
}
async function verify(env, token) {
  const p = String(token || "").split(".");
  if (p.length !== 2 || !await crypto.subtle.verify("HMAC", await keyFor(env), unb64url(p[1]), enc.encode(p[0]))) return null;
  const result = JSON.parse(dec.decode(unb64url(p[0])));
  return Number(result.exp) > Date.now() / 1000 ? result : null;
}
async function gh(url, token, init = {}) {
  const response = await fetch(url, { ...init, headers: {
    Accept: "application/vnd.github+json", Authorization: "Bearer " + token, "X-GitHub-Api-Version": "2022-11-28",
    "content-type": "application/json", ...(init.headers || {})
  } });
  const raw = await response.text(); let data = null;
  try { data = raw ? JSON.parse(raw) : null; } catch {}
  if (!response.ok) throw new Error("GitHub request failed: " + (data?.message || response.status));
  return data;
}
async function appJwt(env) {
  const pem = env.GITHUB_APP_PRIVATE_KEY.replace(/\\n/g, "\n");
  const keyBytes = Uint8Array.from(atob(pem.replace(/-----BEGIN RSA PRIVATE KEY-----|-----END RSA PRIVATE KEY-----|\s/g, "")), c => c.charCodeAt(0));
  const key = await crypto.subtle.importKey("pkcs8", pkcs8(keyBytes), { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["sign"]);
  const now = Math.floor(Date.now() / 1000), header = b64url(enc.encode(JSON.stringify({ alg: "RS256", typ: "JWT" })));
  const body = b64url(enc.encode(JSON.stringify({ iat: now - 60, exp: now + 480, iss: env.GITHUB_APP_ID })));
  const input = header + "." + body;
  return input + "." + b64url(new Uint8Array(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, enc.encode(input))));
}
async function appToken(env) {
  const jwt = await appJwt(env);
  return (await gh("https://api.github.com/app/installations/" + env.GITHUB_INSTALLATION_ID + "/access_tokens", jwt,
    { method: "POST", body: "{}" })).token;
}
function returnPath(env, value) {
  const root = new URL(siteBase(env)), target = new URL(value || (env.SITE_PATH + "/compatibility.html"), root);
  if (target.origin !== root.origin || target.pathname !== env.SITE_PATH + "/compatibility.html") return env.SITE_PATH + "/compatibility.html";
  return target.pathname + target.search;
}
function repositoryResultsPath(env) {
  return String(env.GITHUB_RESULTS_PATH || "site/assets/game-results").replace(/^\/+|\/+$/g, "");
}
async function authStart(env, request) {
  const url = new URL(request.url), state = await sign(env, {
    return_to: returnPath(env, url.searchParams.get("return_to")),
    exp: Math.floor(Date.now() / 1000) + 600, nonce: crypto.randomUUID()
  });
  const callback = env.GITHUB_APP_CALLBACK_URL || (apiBase(request) + "/auth/callback");
  const target = new URL("https://github.com/login/oauth/authorize");
  target.searchParams.set("client_id", env.GITHUB_APP_CLIENT_ID);
  target.searchParams.set("redirect_uri", callback); target.searchParams.set("state", state);
  target.searchParams.set("allow_signup", "false");
  return new Response(null, { status: 302, headers: {
    Location: target.href,
    "Set-Cookie": "__Host-df-oauth-state=" + encodeURIComponent(state) + "; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=600",
    "cache-control": "no-store", "referrer-policy": "no-referrer"
  } });
}
async function authCallback(env, request) {
  const url = new URL(request.url);
  const cookieState = (request.headers.get("Cookie") || "").split(";").map(part => part.trim()).find(part => part.startsWith("__Host-df-oauth-state="))?.split("=").slice(1).join("=") || "";
  const clearState = "__Host-df-oauth-state=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0";
  if (url.searchParams.has("error")) return new Response(null, { status: 302, headers: {
    Location: siteBase(env) + "/compatibility.html?login=cancelled", "Set-Cookie": clearState, "cache-control": "no-store", "referrer-policy": "no-referrer"
  } });
  const state = await verify(env, url.searchParams.get("state")), code = url.searchParams.get("code");
  if (!state || !code || decodeURIComponent(cookieState) !== url.searchParams.get("state")) return new Response("GitHub sign-in could not be verified. Please try again.", { status: 400, headers: { "Set-Cookie": clearState, "cache-control": "no-store", "referrer-policy": "no-referrer" } });
  const response = await fetch("https://github.com/login/oauth/access_token", {
    method: "POST", headers: { Accept: "application/json", "content-type": "application/json" },
    body: JSON.stringify({ client_id: env.GITHUB_APP_CLIENT_ID, client_secret: env.GITHUB_APP_CLIENT_SECRET, code,
      redirect_uri: env.GITHUB_APP_CALLBACK_URL || (apiBase(request) + "/auth/callback") })
  });
  const grant = await response.json();
  if (!response.ok || grant.error || !grant.access_token) return new Response("GitHub sign-in failed. Please try again.", { status: 401, headers: { "Set-Cookie": clearState, "cache-control": "no-store", "referrer-policy": "no-referrer" } });
  const profileResponse = await fetch("https://api.github.com/user", { headers: {
    Accept: "application/vnd.github+json", Authorization: "Bearer " + grant.access_token, "X-GitHub-Api-Version": "2022-11-28"
  } });
  const profile = await profileResponse.json();
  if (!profileResponse.ok || !profile.id || !profile.login) return new Response("GitHub could not verify this account.", { status: 401, headers: { "Set-Cookie": clearState, "cache-control": "no-store", "referrer-policy": "no-referrer" } });
  const session = await sign(env, { sub: profile.id, login: profile.login, exp: Math.floor(Date.now() / 1000) + 2700 });
  const dest = new URL(state.return_to, siteBase(env)); dest.hash = new URLSearchParams({ github_session: session }).toString();
  return new Response(null, { status: 302, headers: { Location: dest.href, "Set-Cookie": clearState, "cache-control": "no-store", "referrer-policy": "no-referrer" } });
}
async function userFromRequest(env, request) {
  const auth = request.headers.get("Authorization") || "";
  return auth.startsWith("Bearer ") ? verify(env, auth.slice(7)) : null;
}
async function appSession(env, request) {
  const body = await request.json();
  const accessToken = String(body.access_token || "");
  if (!accessToken.startsWith("ghu_") || accessToken.length > 1024) return json({ error: "Use a GitHub App device-flow token to sign in." }, 401);
  const response = await fetch("https://api.github.com/user", { headers: {
    Accept: "application/vnd.github+json", Authorization: "Bearer " + accessToken, "X-GitHub-Api-Version": "2022-11-28"
  } });
  const profile = await response.json();
  if (!response.ok || !profile.id || !profile.login) return json({ error: "GitHub could not verify this account." }, 401);
  const expires = Math.floor(Date.now() / 1000) + 2700;
  const session = await sign(env, { sub: profile.id, login: profile.login, exp: expires });
  return json({ session, login: profile.login, expires_at: expires });
}
function field(form, name, label, regex) {
  const value = String(form.get(name) || "").trim();
  if (!regex.test(value)) throw new Error(label + " is missing or invalid.");
  return value;
}
async function openPullRequest(env, record, logBytes, fileName, user, token) {
  const base = env.GITHUB_BASE_BRANCH || "master";
  const repo = "https://api.github.com/repos/" + env.GITHUB_OWNER + "/" + env.GITHUB_REPOSITORY;
  const openPulls = await gh(repo + "/pulls?state=open&per_page=100", token);
  if (openPulls.some(pull => pull.title === "Successful test: " + record.game_id)) {
    throw new Error("A successful test report is already waiting for review for " + record.game_id + ".");
  }
  const ref = await gh(repo + "/git/ref/heads/" + encodeURIComponent(base), token);
  const parent = await gh(repo + "/git/commits/" + ref.object.sha, token);
  const resultPath = repositoryResultsPath(env) + "/" + record.game_id + ".json";
  const logPath = "site/assets/game-logs/" + record.game_id + "-" + crypto.randomUUID() + (fileName.toLowerCase().endsWith(".txt") ? ".txt" : ".log");
  const logBlob = await gh(repo + "/git/blobs", token, { method: "POST", body: JSON.stringify({
    content: base64(logBytes), encoding: "base64"
  }) });
  const blob = await gh(repo + "/git/blobs", token, { method: "POST", body: JSON.stringify({
    content: JSON.stringify({ schema_version: 1, game: record }, null, 2) + "\n", encoding: "utf-8"
  }) });
  const tree = await gh(repo + "/git/trees", token, { method: "POST", body: JSON.stringify({
    base_tree: parent.tree.sha, tree: [
      { path: resultPath, mode: "100644", type: "blob", sha: blob.sha },
      { path: logPath, mode: "100644", type: "blob", sha: logBlob.sha }
    ]
  }) });
  const commit = await gh(repo + "/git/commits", token, { method: "POST", body: JSON.stringify({
    message: "Add game test result: " + record.game_id, tree: tree.sha, parents: [ref.object.sha]
  }) });
  const branch = "digital-forge-success/" + record.game_id.toLowerCase() + "-" + crypto.randomUUID().slice(0, 8);
  await gh(repo + "/git/refs", token, { method: "POST", body: JSON.stringify({ ref: "refs/heads/" + branch, sha: commit.sha }) });
  const logUrl = "https://github.com/" + env.GITHUB_OWNER + "/" + env.GITHUB_REPOSITORY
    + "/blob/" + branch + "/" + logPath;
  const body = [
    "## Successful game test submission", "", "Merging this pull request publishes the test result on the website.", "",
    "- **Game ID:** " + record.game_id, "- **Console:** " + record.console, "- **Format:** " + record.format,
    "- **Digital Forge version:** " + (record.digital_forge_version || "Not provided"),
    "- **Test environment:** " + (record.environment || "Not provided"), "- **Tested at (UTC):** " + record.tested_at,
    "- **Submitted by:** @" + user.login, "- **Game log:** [Review the attached log](" + logUrl + ")",
    "", "The game log is attached to this pull request in the public Digital Forge repository, so it is visible before and after merge. Review it for private information before submitting."
  ].join("\n");
  return gh(repo + "/pulls", token, { method: "POST", body: JSON.stringify({
    title: "Successful test: " + record.game_id, head: branch, base, body
  }) });
}
async function submit(env, request) {
  const user = await userFromRequest(env, request);
  if (!user) return json({ error: "Sign in with GitHub before submitting." }, 401);
  try {
    const contentLength = Number(request.headers.get("Content-Length") || 0);
    if (contentLength > MAX_LOG + 128 * 1024) return json({ error: "The submission is larger than the 5 MB game log limit." }, 413);
    const form = await request.formData();
    const id = String(form.get("game_id") || "").trim().toUpperCase();
    if (!ID_RE.test(id)) throw new Error("Game ID is missing or invalid.");
    const console = field(form, "console", "Console", FIELD_RE);
    const format = field(form, "format", "Format", FIELD_RE);
    const version = String(form.get("digital_forge_version") || "").trim().slice(0, 48);
    const environment = String(form.get("environment") || "").trim().slice(0, 100);
    if ((version && !FIELD_RE.test(version)) || !OPTIONAL_RE.test(environment)) throw new Error("Version or test environment contains unsupported characters.");
    const file = form.get("game_log");
    if (!(file instanceof File) || file.size < 1 || file.size > MAX_LOG) throw new Error("Attach a non-empty game log no larger than 5 MB.");
    if (!/\.(log|txt)$/i.test(file.name) || !["text/plain", "application/octet-stream", ""].includes(file.type)) throw new Error("Attach a .log or .txt game log.");
    const log = new Uint8Array(await file.arrayBuffer()); dec.decode(log);
    const token = await appToken(env), repo = "https://api.github.com/repos/" + env.GITHUB_OWNER + "/" + env.GITHUB_REPOSITORY;
    const resultsPath = repositoryResultsPath(env);
    const base = env.GITHUB_BASE_BRANCH || "master", path = resultsPath + "/" + id + ".json";
    const exists = await fetch(repo + "/contents/" + path + "?ref=" + encodeURIComponent(base), {
      headers: { Accept: "application/vnd.github+json", Authorization: "Bearer " + token, "X-GitHub-Api-Version": "2022-11-28" }
    });
    if (exists.ok) return json({ error: "A result already exists for " + id + "." }, 409);
    if (exists.status !== 404) throw new Error("Could not verify existing results (GitHub HTTP " + exists.status + ").");
    const catalogResponse = await fetch(repo + "/contents/" + resultsPath + ".json?ref=" + encodeURIComponent(base), {
      headers: { Accept: "application/vnd.github+json", Authorization: "Bearer " + token, "X-GitHub-Api-Version": "2022-11-28" }
    });
    if (catalogResponse.ok) {
      const metadata = await catalogResponse.json();
      const catalogBytes = Uint8Array.from(atob(String(metadata.content || "").replace(/\n/g, "")), c => c.charCodeAt(0));
      const catalog = JSON.parse(dec.decode(catalogBytes));
      if (Array.isArray(catalog.games) && catalog.games.some(item => String(item.game_id).toUpperCase() === id)) return json({ error: "A result already exists for " + id + "." }, 409);
    } else if (catalogResponse.status !== 404) throw new Error("Could not verify the existing result catalog (GitHub HTTP " + catalogResponse.status + ").");
    const record = { game_id: id, status: "working", tested_at: new Date().toISOString(), console, format,
      ...(version ? { digital_forge_version: version } : {}), ...(environment ? { environment } : {}),
      submitted_by: user.login };
    const pr = await openPullRequest(env, record, log, file.name, user, token);
    return json({ ok: true, pull_request: pr.html_url, number: pr.number, game_id: id }, 201);
  } catch (error) {
    return json({ error: error instanceof Error ? error.message : "Submission failed." }, 400);
  }
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url), headers = cors(env, request);
    const apiPrefix = String(env.API_PATH_PREFIX || "").replace(/\/$/, "");
    if (apiPrefix && url.pathname.startsWith(apiPrefix + "/")) {
      url.pathname = url.pathname.slice(apiPrefix.length);
      request = new Request(url, request);
    }
    if (request.method === "OPTIONS") return new Response(null, { status: 204, headers });
    try {
      if (url.pathname === "/auth/start" && request.method === "GET") return await authStart(env, request);
      if (url.pathname === "/auth/callback" && request.method === "GET") return await authCallback(env, request);
      if (url.pathname === "/api/me" && request.method === "GET") {
        const user = await userFromRequest(env, request);
        return user ? json({ login: user.login, expires_at: user.exp }, 200, headers) : json({ error: "Sign in with GitHub." }, 401, headers);
      }
      if (url.pathname === "/api/app-session" && request.method === "POST") {
        const response = await appSession(env, request);
        for (const [k, v] of Object.entries(headers)) response.headers.set(k, v);
        return response;
      }
      if (url.pathname === "/api/success-reports" && request.method === "POST") {
        const response = await submit(env, request); for (const [k, v] of Object.entries(headers)) response.headers.set(k, v); return response;
      }
      return json({ error: "Not found." }, 404, headers);
    } catch (error) {
      return json({ error: error instanceof Error ? error.message : "Request failed." }, 500, headers);
    }
  }
};
