import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const source = await readFile(new URL("../assets/search.js", import.meta.url), "utf8");
const listeners = {};
const input = { value: "", disabled: true, focus() { this.focused = true; } };
const inputLabel = { textContent: "Search game title or ID" };
const mode = { value: "", focus() { this.focused = true; }, addEventListener: (name, callback) => { listeners[`mode-${name}`] = callback; } };
const homeButton = { addEventListener: (name, callback) => { listeners[`home-${name}`] = callback; } };
const searchHelp = { textContent: "" };
const output = { hidden: true, innerHTML: "" };
const issueResults = { innerHTML: "" };
const form = { addEventListener: (name, callback) => { listeners[name] = callback; } };
let apiResponse = { ok: true, status: 200, json: async () => ({ items: [] }) };
let resultResponse = { ok: true, status: 200, json: async () => ({ schema_version: 1, games: [] }) };
let individualResponse = { ok: false, status: 404, json: async () => ({}) };
const requests = [];
const opened = [];
const document = {
  baseURI: "https://digital-forge.example.test/",
  querySelector: selector => ({ "#search-form": form, '#site-search': input, 'label[for="site-search"]': inputLabel, "#search-mode": mode, "#home-button": homeButton, "#search-help": searchHelp, "#search-results": output })[selector] || null,
  querySelectorAll: () => [],
  getElementById: id => id === "official-emulator-issues" || (id === "community-issues" && output.innerHTML.includes('id="community-issues"')) ? issueResults : null
};
const context = {
  URL, URLSearchParams, AbortController, setTimeout, clearTimeout, document,
  location: { search: "" }, history: { replaceState() {} }, window: { open: (...args) => { opened.push(args); }, location: { assign: url => { opened.push([url]); } } },
  fetch: async url => {
    requests.push(String(url));
    if (String(url).includes("assets/game-results/")) return individualResponse;
    return String(url).includes("game-results.json") ? resultResponse : apiResponse;
  }
};
vm.runInNewContext(source, context, { filename: "search.js" });

assert.equal(mode.value, "", "homepage should begin without a selected filter");
assert.equal(input.disabled, true, "search field should be disabled without a selected filter");
assert.match(searchHelp.textContent, /choose Guides or Games/i);
assert.doesNotMatch(source, /emulatorLookups|mode.*emulators/i, "homepage search must not include an emulator mode");
input.value = "CUSA08519";
listeners.submit({ preventDefault() {} });
assert.equal(requests.length, 0, "search without a selected filter should not run a query");
assert.equal(mode.focused, true, "search without a filter should focus the filter control");
listeners["home-click"]();
assert.equal(opened.at(-1)[0], "https://digital-forge.example.test/index.html", "Home should navigate to the clean site root");
input.value = "";
mode.value = "";

async function submit(query, searchMode = "games") {
  input.value = query;
  if (mode.value !== searchMode) {
    mode.value = searchMode;
    listeners["mode-change"]();
  }
  assert.equal(input.disabled, false, "search field should be enabled after selecting a filter");
  output.innerHTML = "";
  issueResults.innerHTML = "";
  output.hidden = true;
  listeners.submit({ preventDefault() {} });
  if (searchMode === "games") {
    for (let attempt = 0; attempt < 200 && !issueResults.innerHTML; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 25));
    }
  }
  return output.innerHTML;
}

let html = "";
assert.match(await submit("PS4 Itemflow", "guides"), /Itemzflow guide/);
assert.match(await submit("PS4 FPKG", "guides"), /FPKG guide/);
assert.match(await submit("PS3 .PKG", "guides"), /No guide matched/);
assert.doesNotMatch(await submit("PS3 .PKG", "guides"), /PKG guide/);
assert.match(await submit("PS4", "guides"), /Itemzflow guide/);
assert.match(await submit("PS4", "guides"), /FPKG guide/);
assert.match(await submit("FPKG", "guides"), /FPKG guide/);
assert.match(await submit("Xbox 360 ISO2GOD", "guides"), /ISO2GOD guide/);
assert.match(await submit("PS3 FPKG", "guides"), /No guide matched/);

requests.length = 0;
html = await submit("CUSA08519", "guides");
assert.match(html, /No guide matched/);
assert.doesNotMatch(html, /community reports/);
assert.equal(requests.length, 0, "Guides mode should search documentation only");

html = await submit("Itemflow", "games");
assert.doesNotMatch(html, /Itemzflow guide/, "Games mode should not return guide content");
assert.match(html, /Digital Forge community reports/);
assert.match(issueResults.innerHTML, /Test status: Unknown/);

requests.length = 0;
html = await submit("CUSA08519");
assert.match(html, /Digital Forge community reports/);
assert.match(issueResults.innerHTML, /Test status: Unknown/);
assert.doesNotMatch(issueResults.innerHTML, /game should work/i);
assert.match(issueResults.innerHTML, /CUSA08519/);
assert.equal((issueResults.innerHTML.match(/class="result-card/g) || []).length, 1, "no-report result and report actions should appear in one card");
assert.doesNotMatch(issueResults.innerHTML, /Submit a successful test result|Report a game result/);
assert.match(decodeURIComponent(requests.find(url => url.includes("api.github.com"))), /repo:rileyadams05\/digital-forge is:issue CUSA08519/);

resultResponse = { ok: true, status: 200, json: async () => ({ schema_version: 1, games: [{ game_id: "CUSA08519", status: "working", tested_at: "2026-10-17", environment: "PS4 11.00", digital_forge_version: "1.0" }] }) };
individualResponse = { ok: true, status: 200, json: async () => ({ schema_version: 1, game: { game_id: "CUSA08519", status: "working", tested_at: "2026-10-17", environment: "PS4 11.00", digital_forge_version: "1.0" } }) };
html = await submit("CUSA08519");
assert.match(issueResults.innerHTML, /Test status: Tested working/);
assert.match(issueResults.innerHTML, /CUSA08519/);
assert.doesNotMatch(issueResults.innerHTML, /Report a game failure/);
resultResponse = { ok: true, status: 200, json: async () => ({ schema_version: 1, games: [] }) };
individualResponse = { ok: true, status: 200, json: async () => ({ schema_version: 1, game: { game_id: "CUSA08519", status: "working", tested_at: "2026-10-17" } }) };
html = await submit("CUSA08519");
assert.match(issueResults.innerHTML, /Test status: Tested working/, "a merged per-game review file should publish its approved result");
individualResponse = { ok: false, status: 404, json: async () => ({}) };

apiResponse = { ok: true, status: 200, json: async () => ({ items: [{
  title: "[Game failure] CUSA08519 crash on launch",
  body: "### Observed game failure\nGame crashed during play\n### Game ID\nCUSA08519",
  html_url: "https://github.com/rileyadams05/digital-forge/issues/123",
  created_at: "2026-10-16T10:00:00Z",
  number: 123
}] }) };
html = await submit("CUSA08519");
assert.match(issueResults.innerHTML, /Test status: Problem reported/);
assert.match(issueResults.innerHTML, /recommend not using this game/i);
assert.match(issueResults.innerHTML, /Report a game failure/);
assert.equal((issueResults.innerHTML.match(/class="result-card/g) || []).length, 1, "failure status and its report link should appear in one card");
apiResponse = { ok: true, status: 200, json: async () => ({ items: [] }) };

requests.length = 0;
html = await submit("BLUS30443");
assert.match(html, /Digital Forge community reports/);
assert.equal(decodeURIComponent(requests.find(url => url.includes("api.github.com"))).match(/repo:rileyadams05\/digital-forge is:issue ([^&]+)/)?.[1], "BLUS30443", "Games search must submit only the entered game ID");

apiResponse = { ok: false, status: 403, json: async () => ({}) };
html = await submit("CUSA08519");
assert.match(issueResults.innerHTML, /Test status: Unknown/);
assert.match(issueResults.innerHTML, /rate limit or access restriction/);
assert.equal((issueResults.innerHTML.match(/class="result-card/g) || []).length, 1, "API errors should remain in one status card");

console.log("Search smoke checks passed: Guides and Games filters, game status reporting, and API failure states.");
