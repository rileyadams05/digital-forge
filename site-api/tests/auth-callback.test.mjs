import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import worker from "../src/index.mjs";

const env = {
  SESSION_SECRET: "test-session-secret-with-more-than-32-bytes",
  SITE_ORIGIN: "digital-forge.vortex-prime-emu.com",
  SITE_PATH: "",
  GITHUB_APP_CALLBACK_URL: "https://digital-forge-api.vortex-prime-emu.com/auth/callback",
  GITHUB_APP_CLIENT_ID: "test-client-id",
  GITHUB_APP_CLIENT_SECRET: "test-client-secret",
};

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

async function startLogin() {
  const response = await worker.fetch(
    new Request("https://digital-forge-api.vortex-prime-emu.com/auth/start"),
    env,
  );
  assert.equal(response.status, 302);
  const authorizeUrl = new URL(response.headers.get("Location"));
  const state = authorizeUrl.searchParams.get("state");
  const cookie = response.headers.get("Set-Cookie").split(";", 1)[0];
  return { state, cookie };
}

async function callback(state, cookie) {
  const url = new URL("https://digital-forge-api.vortex-prime-emu.com/auth/callback");
  url.searchParams.set("code", "test-code");
  url.searchParams.set("state", state);
  return worker.fetch(new Request(url, { headers: { Cookie: cookie } }), env);
}

test("non-JSON OAuth rejection returns a clean sign-in error", async () => {
  const { state, cookie } = await startLogin();
  globalThis.fetch = async () => new Response("Request forbidden by test fixture", {
    status: 403,
    headers: { "content-type": "text/plain" },
  });

  const response = await callback(state, cookie);
  assert.equal(response.status, 401);
  assert.equal(await response.text(), "GitHub sign-in failed. Please try again.");
  assert.match(response.headers.get("Set-Cookie"), /Max-Age=0/);
});

test("form-encoded OAuth errors are handled without parsing exceptions", async () => {
  const { state, cookie } = await startLogin();
  globalThis.fetch = async () => new Response("error=bad_verification_code", {
    status: 200,
    headers: { "content-type": "application/x-www-form-urlencoded" },
  });

  const response = await callback(state, cookie);
  assert.equal(response.status, 401);
  assert.equal(await response.text(), "GitHub sign-in failed. Please try again.");
});

test("valid JSON OAuth response completes the callback and creates a session", async () => {
  const { state, cookie } = await startLogin();
  globalThis.fetch = async (url, init) => {
    if (String(url) === "https://github.com/login/oauth/access_token") {
      assert.equal(init.method, "POST");
      assert.equal(init.headers["content-type"], "application/x-www-form-urlencoded");
      const form = new URLSearchParams(init.body);
      assert.equal(form.get("client_id"), "test-client-id");
      assert.equal(form.get("client_secret"), "test-client-secret");
      assert.equal(form.get("code"), "test-code");
      assert.equal(form.get("redirect_uri"), env.GITHUB_APP_CALLBACK_URL);
      return Response.json({ access_token: "test-user-token" });
    }
    if (String(url) === "https://api.github.com/user") {
      return Response.json({ id: 123, login: "test-user" });
    }
    throw new Error("Unexpected test request destination");
  };

  const response = await callback(state, cookie);
  assert.equal(response.status, 302);
  const destination = new URL(response.headers.get("Location"));
  assert.equal(destination.origin, "https://digital-forge.vortex-prime-emu.com");
  assert.equal(destination.pathname, "/compatibility.html");
  assert.ok(new URLSearchParams(destination.hash.slice(1)).get("github_session"));
  assert.match(response.headers.get("Set-Cookie"), /Max-Age=0/);
});
