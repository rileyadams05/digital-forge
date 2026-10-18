if (typeof document.addEventListener === "function") {
  document.addEventListener("keydown", event => {
    if (event.key === "Tab") document.documentElement.classList.add("keyboard-focus");
  }, true);
  document.addEventListener("pointerdown", () => {
    document.documentElement.classList.remove("keyboard-focus");
  }, true);
}

const PROJECT = "https://github.com/rileyadams05/digital-forge";
const REPORT_FORM = `${PROJECT}/issues/new?template=compatibility-report.yml`;
const GAME_RESULTS_BASE = "https://raw.githubusercontent.com/rileyadams05/digital-forge/master/site/assets";
const GAME_RESULTS_URL = GAME_RESULTS_BASE + "/game-results.json";
const guides = [
  { title: "Itemzflow guide", url: "guides/itemzflow.html", platforms: ["ps4"], formats: ["itemzflow", "itemflow", "itemflows", "item flows"], keys: ["dump all", "gp4", "param.sfo"], summary: "Prepare and import an Itemzflow base-game and update dump folder for the existing PS4 single-package workflow." },
  { title: "FPKG guide", url: "guides/fpkg.html", platforms: ["ps4"], formats: ["fpkg", "fake pkg", "fakepkg", "fake package"], keys: [], summary: "PS4 Fake PKG inputs, configured packaging tools, validation, merge path and limitations." },
  { title: "PKG guide", url: "guides/pkg.html", platforms: ["ps3", "ps4"], formats: ["pkg", ".pkg", "package"], keys: ["retail pkg", "ps3 pkg", "ps4 pkg"], summary: "PlayStation package formats and platform-specific processing limits." },
  { title: "ISO2GOD guide", url: "guides/iso2god.html", platforms: ["xbox360"], formats: ["iso2god", "god", "games on demand"], keys: ["xbox 360 iso", "xbox360"], summary: "Xbox 360 ISO and Games on Demand workflow, inputs, storage and troubleshooting." },
  { title: "Troubleshooting and compatibility reports", url: "compatibility.html", platforms: [], formats: [], keys: ["troubleshooting", "diagnostic", "log", "issue", "github", "report", "failure", "error", "compatibility"], summary: "Search prior reports, preserve version details and submit a report with a reviewed diagnostic log." }
];
const consoleNames = [
  { id: "ps4", aliases: ["ps4", "playstation 4"] },
  { id: "ps3", aliases: ["ps3", "playstation 3"] },
  { id: "ps2", aliases: ["ps2", "playstation 2"] },
  { id: "ps1", aliases: ["ps1", "psx", "playstation 1", "playstation"] },
  { id: "psp", aliases: ["psp", "playstation portable"] },
  { id: "vita", aliases: ["vita", "ps vita", "playstation vita"] },
  { id: "xbox360", aliases: ["x360", "xbox 360", "xbox360"] },
  { id: "xbox_orig", aliases: ["original xbox", "og xbox", "xbox"] },
  { id: "nes", aliases: ["nes", "famicom disk system", "fds", "famicom"] },
  { id: "snes", aliases: ["snes", "super nintendo", "super famicom", "satellaview", "sufami turbo"] },
  { id: "n64", aliases: ["n64", "nintendo 64", "64dd", "nintendo 64dd"] },
  { id: "gb", aliases: ["gb", "game boy", "gameboy", "gbc", "game boy color", "gameboy color"] },
  { id: "gba", aliases: ["gba", "game boy advance", "gameboy advance"] },
  { id: "nds", aliases: ["nds", "ds", "nintendo ds", "dsi", "nintendo dsi"] },
  { id: "3ds", aliases: ["3ds", "nintendo 3ds"] },
  { id: "gamecube", aliases: ["gamecube", "game cube", "gc", "nintendo gamecube"] },
  { id: "wii", aliases: ["wii", "nintendo wii"] },
  { id: "wii_u", aliases: ["wii u", "wiiu", "nintendo wii u"] },
  { id: "dreamcast", aliases: ["dreamcast", "dc", "sega dreamcast"] },
  { id: "saturn", aliases: ["saturn", "sega saturn"] },
  { id: "32x", aliases: ["32x", "sega 32x"] }
];
const jumpToId = (base, id) => `${base}#:~:text=${encodeURIComponent(id)}`;
const emulatorLookups = [
  { aliases: ["playstation 1", "ps1", "psx"], name: "DuckStation", url: id => jumpToId("https://docs.google.com/spreadsheets/d/e/2PACX-1vRE0jjiK_aldpICoy5kVQlpk2f81Vo6P4p9vfg4d7YoTOoDlH4PQHoXjTD2F7SdN8SSBLoEAItaIqQo/pubhtml", id) },
  { aliases: ["playstation 2", "ps2"], name: "PCSX2", url: id => jumpToId("https://pcsx2.net/compat/", id) },
  { aliases: ["playstation 3", "ps3"], name: "RPCS3", url: id => `https://www.rpcs3.net/compatibility?g=${encodeURIComponent(id)}` },
  { aliases: ["playstation 4", "ps4"], name: "shadPS4", url: id => `https://github.com/shadps4-compatibility/shadps4-game-compatibility/issues?q=${encodeURIComponent(`is:issue ${id}`)}` },
  { aliases: ["playstation vita", "ps vita", "vita"], name: "Vita3K", url: id => `https://github.com/Vita3K/compatibility/issues?q=${encodeURIComponent(`is:issue ${id}`)}` },
  { aliases: ["original xbox", "og xbox", "xbox"], name: "xemu", url: id => `https://xemu.app/titles/${encodeURIComponent(id.toLowerCase())}/` },
  { aliases: ["xbox 360", "xbox360", "x360"], name: "Xenia Canary", url: id => `https://github.com/xenia-canary/game-compatibility/issues?q=${encodeURIComponent(`is:issue ${id}`)}` },
  { aliases: ["nintendo 3ds", "3ds"], name: "Azahar", url: id => `https://github.com/azahar-emu/compatibility-list/issues?q=${encodeURIComponent(`is:issue ${id}`)}` },
];
const consolesWithoutVerifiedList = [
  "ps5", "playstation 5", "switch", "nintendo switch", "n64", "nintendo 64", "64dd", "nintendo 64dd", "virtual_boy", "virtual boy", "pokemon_mini", "pokemon mini", "sg1000", "sega sg-1000", "master_system", "master system", "sega master system", "game_gear", "game gear", "sega game gear", "mega_drive", "mega drive", "genesis", "sega cd", "mega-cd", "sega pico", "pico", "naomi", "naomi2", "naomi 2", "atomiswave", "system_sp", "system sp", "pc_engine", "pc engine", "turbografx-16", "supergrafx", "pc_engine_cd", "pc engine cd", "turbografx-cd", "pc_fx", "pc-fx", "neo_geo", "neo geo", "neo geo aes", "neo geo mvs", "neo_geo_cd", "neo geo cd", "ngp", "neo geo pocket", "ngpc", "neo geo pocket color", "atari_2600", "atari 2600", "atari_5200", "atari 5200", "atari_7800", "atari 7800", "atari_lynx", "atari lynx", "atari_jaguar", "atari jaguar", "atari_jaguar_cd", "jaguar cd", "wonderswan", "wonderswan_color", "wonderswan color", "3do", "colecovision", "intellivision", "odyssey2", "odyssey 2", "videopac", "vectrex", "channel_f", "fairchild channel f", "supervision", "mega_duck", "mega duck", "cougar boy", "philips_cdi", "philips cd-i", "gx4000", "cassette_vision", "cassette vision", "super_cassette_vision", "super cassette vision", "creativision", "my_vision", "my vision", "atari_xegs", "xegs", "arcadia_2001", "arcadia 2001", "vc_4000", "vc 4000", "elektor_tvc", "elektor tv games computer", "arduboy", "amiga_cd32", "amiga cd32", "dingoo_a320", "dingoo a320", "gemei a330", "sega_model_3", "sega model 3", "nuon", "pece", "p/ece", "pocketstation", "sega_vmu", "sega vmu", "n_gage", "n-gage", "nokia n-gage", "tamagotchi_p1", "tamagotchi p1", "uzebox", "wii_u", "spmp8000", "native32", "cps1", "cps2", "cps3", "handheld_electronic", "handheld electronic games"
];

const form = document.querySelector("#search-form");
const input = document.querySelector("#site-search");
const inputLabel = document.querySelector('label[for="site-search"]');
const mode = document.querySelector("#search-mode");
const homeButton = document.querySelector("#home-button");
const searchHelp = document.querySelector("#search-help");
const output = document.querySelector("#search-results");
const params = new URLSearchParams(location.search);
let currentSearch = 0;
if (params.has("q") && input) input.value = params.get("q");
if (["guides", "games", "emulators"].includes(params.get("mode")) && mode) mode.value = params.get("mode");

function updateSearchMode() {
  const selectedMode = mode?.value || "";
  const guideMode = selectedMode === "guides";
  const emulatorMode = selectedMode === "emulators";
  if (input) input.disabled = !selectedMode;
  if (inputLabel) inputLabel.textContent = guideMode ? "Search Digital Forge guides" : emulatorMode ? "Search console and game ID" : selectedMode === "games" ? "Search game ID" : "Search";
  if (input) input.placeholder = guideMode ? "Console short name + format, e.g. PS4 FPKG…" : emulatorMode ? "Console and game ID, e.g. PS3 BLUS30443…" : selectedMode === "games" ? "Enter a game ID…" : "Choose a filter to start…";
  if (searchHelp) searchHelp.textContent = !selectedMode
    ? "Choose Guides, Games, or Emulators to begin."
    : emulatorMode
    ? "Emulators: enter the console name or short name, followed by the game ID, to open its emulator compatibility lookup."
    : guideMode
      ? "Guides: enter the console short name followed by the format, such as PS4 FPKG or Xbox 360 ISO2GOD."
    : "Games searches approved test results and GitHub failure reports by game ID.";
  if (searchHelp) searchHelp.textContent += " Press Enter to search.";
}
function clearResults() {
  if (output) {
    output.hidden = true;
    output.innerHTML = "";
  }
}
updateSearchMode();

function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, ch => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]);
}
function safeExternal(url) {
  try { const parsed = new URL(url); return parsed.protocol === "https:" && parsed.hostname === "github.com" ? parsed.href : null; }
  catch { return null; }
}
function escapeRegExp(value) { return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"); }
function card(tag, title, summary, url, label, external = false) {
  const href = external ? safeExternal(url) : url;
  if (!href) return "";
  return `<article class="result-card"><div class="tag">${esc(tag)}</div><h3>${esc(title)}</h3><p>${esc(summary)}</p><a href="${esc(href)}"${external ? ' target="_blank" rel="noopener noreferrer"' : ""}>${esc(label)}${external ? " ↗" : ""}</a></article>`;
}
function phraseTokens(value) {
  return String(value).toLowerCase().replace(/\bitemflows?\b/g, "itemzflow").match(/[\p{L}\p{N}]+/gu) || [];
}
function matchesPhrase(query, phrase) {
  const words = phraseTokens(query);
  const expected = phraseTokens(phrase);
  return expected.length > 0 && words.some((_, index) => expected.every((word, offset) => words[index + offset] === word));
}
function searchGuides(query) {
  const detectedConsoles = consoleNames.filter(consoleName => consoleName.aliases.some(alias => matchesPhrase(query, alias))).map(consoleName => consoleName.id);
  const matching = guides.filter(guide => {
    const matchedFormat = guide.formats.some(format => matchesPhrase(query, format));
    const matchedTopic = guide.keys.some(key => matchesPhrase(query, key));
    if (!matchedFormat && !matchedTopic) return false;
    return !detectedConsoles.length || !guide.platforms.length || detectedConsoles.some(consoleId => guide.platforms.includes(consoleId));
  });
  return { detectedConsoles, matching };
}
function run(rawQuery, searchMode = mode?.value || "games") {
  const query = rawQuery.trim();
  const searchId = ++currentSearch;
  if (!query || !output) return;
  output.hidden = false;
  const normalized = query.toLowerCase().replace(/\bitemflows?\b/g, "itemzflow");
  if (searchMode === "guides") {
    const { detectedConsoles, matching: matchingGuides } = searchGuides(normalized);
    let guideHtml = `<h2>Guide results for “${esc(query)}”</h2>`;
    for (const guide of matchingGuides) guideHtml += card("Digital Forge guide", guide.title, guide.summary, guide.url, "Open guide");
    if (!matchingGuides.length) {
      const prompt = detectedConsoles.length
        ? "Add the format or workflow you used after the console name, for example PS4 PKG or Xbox 360 ISO2GOD."
        : "Enter the console short name followed by a format or workflow, for example PS4 FPKG, PS4 Itemzflow or Xbox 360 ISO2GOD.";
      guideHtml += '<p class="callout">No guide matched “' + esc(query) + '”. ' + esc(prompt) + '</p>';
    }
    output.innerHTML = guideHtml;
    return;
  }
  let html = `<h2>Game results for “${esc(query)}”</h2>`;
  const fallback = `${PROJECT}/issues?q=${encodeURIComponent(`is:issue ${query}`)}`;
  html += `<section id="community-issues"><article class="result-card"><div class="tag">Digital Forge community reports</div><h3>Checking test status for ${esc(query)}</h3><p>Approved successful tests are listed on this site. GitHub Issues are used for game failures.</p></article></section>`;
  output.innerHTML = html;
  loadGameStatus(query, "community-issues", fallback, searchId);
}

async function loadGameStatus(id, targetId, fallback, searchId) {
  const target = document.getElementById(targetId);
  if (!target) return;
  const search = `repo:rileyadams05/digital-forge is:issue ${id}`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 9000);
  try {
    const [resultsResponse, issuesResponse] = await Promise.all([
      fetch(GAME_RESULTS_URL, { signal: controller.signal }),
      fetch(`https://api.github.com/search/issues?q=${encodeURIComponent(search)}&per_page=100&sort=created&order=desc`, {
        headers: { Accept: "application/vnd.github+json" }, signal: controller.signal
      })
    ]);
    if (searchId !== currentSearch) return;
    let approvedResult = null;
    let resultsUnavailable = !resultsResponse.ok;
    if (resultsResponse.ok) {
      try {
        const catalog = await resultsResponse.json();
        approvedResult = (Array.isArray(catalog.games) ? catalog.games : []).find(entry =>
          entry && typeof entry.game_id === "string" && entry.game_id.toUpperCase() === id.toUpperCase() && entry.status === "working"
        ) || null;
        const individualResponse = await fetch(GAME_RESULTS_BASE + "/game-results/" + encodeURIComponent(id.toUpperCase()) + ".json", { signal: controller.signal });
        if (individualResponse.ok) {
          const individual = await individualResponse.json();
          if (individual.game && individual.game.game_id?.toUpperCase() === id.toUpperCase() && individual.game.status === "working") approvedResult = individual.game;
        } else if (individualResponse.status !== 404) resultsUnavailable = true;
      } catch { resultsUnavailable = true; }
    }
    let entries = [];
    let issuesUnavailable = !issuesResponse.ok;
    let issueReason = issuesResponse.status === 403 || issuesResponse.status === 429
      ? "GitHub rate limit or access restriction"
      : `GitHub returned HTTP ${issuesResponse.status}`;
    if (issuesResponse.ok) {
      const data = await issuesResponse.json();
      entries = (data.items || []).filter(issue => !issue.pull_request && safeExternal(issue.html_url));
      if ((data.items || []).length && !entries.length) throw new Error("GitHub results could not be safely displayed");
      const exactId = new RegExp(`(^|[^A-Z0-9])${escapeRegExp(id)}($|[^A-Z0-9])`, "i");
      entries = entries.filter(issue => {
        const body = String(issue.body || "");
        const title = String(issue.title || "");
        const failureReport = /^\[Game failure\]/i.test(title)
          || /###\s*Observed game failure\s*\n+/i.test(body)
          || /###\s*Game test status\s*\n+Tested;?\s*a problem was found/i.test(body);
        return failureReport && exactId.test(`${title} ${body}`);
      });
    }
    const newestFailure = entries.reduce((latest, issue) => {
      const created = Date.parse(issue.created_at || "") || 0;
      return created > latest ? created : latest;
    }, 0);
    const passDate = approvedResult ? Date.parse(approvedResult.tested_at || "") || 0 : 0;
    const failedAfterPass = entries.length > 0 && (!approvedResult || newestFailure > passDate);
    const status = failedAfterPass ? "Problem reported" : approvedResult ? "Tested working" : "Unknown";
    const statusMessage = failedAfterPass
      ? "A failure report matches this game ID. We recommend not using this game until someone has fixed it and a newer successful test is recorded."
      : approvedResult
        ? "This game has an approved successful test result. Results can vary by game version and test environment."
        : `No approved test result was found for game ID <strong>${esc(id)}</strong>, so its status is unknown.`;
    const testDetails = approvedResult
      ? `<p>${approvedResult.tested_at ? `Tested: ${esc(approvedResult.tested_at)}. ` : ""}${approvedResult.environment ? `Environment: ${esc(approvedResult.environment)}. ` : ""}${approvedResult.digital_forge_version ? `Digital Forge: ${esc(approvedResult.digital_forge_version)}.` : ""}</p>`
      : "";
    const failureLinks = entries.map(issue => `<li><a href="${esc(safeExternal(issue.html_url))}" target="_blank" rel="noopener noreferrer">${esc(issue.title || `Failure report #${issue.number}`)}</a></li>`).join("");
    const links = failedAfterPass
      ? `<p>Include game ID <strong>${esc(id)}</strong>, the failing game version and environment, and the reviewed Digital Forge game log in the failure report.</p><p><a href="${esc(REPORT_FORM)}" target="_blank" rel="noopener noreferrer">Report a game failure ↗</a></p><ul>${failureLinks}</ul>`
      : approvedResult
        ? `<p>Successful test results are published after project review. GitHub Issues are for game failures only. <a href="${esc(fallback)}" target="_blank" rel="noopener noreferrer">Review failure reports ↗</a></p>`
        : `<p>If you have already tested this game and it worked, you can submit the result for project review. If it failed, report the problem with the game ID and a reviewed Digital Forge game log.</p><p><a href="compatibility.html?game=${encodeURIComponent(id)}#success-report">Submit a successful test</a> · <a href="${esc(REPORT_FORM)}" target="_blank" rel="noopener noreferrer">Report a game failure ↗</a></p>`;
    const availability = issuesUnavailable
      ? `<p class="muted-note">Failure reports could not be checked right now (${esc(issueReason)}); this result may not include recent reports.</p>`
      : resultsUnavailable
        ? "<p class=\"muted-note\">The approved test-result list could not be loaded right now.</p>"
        : "";
    target.innerHTML = `<article class="result-card"><div class="tag">Digital Forge community reports</div><h3>Test status: ${status}</h3><p>${statusMessage}</p>${testDetails}${availability}${links}</article>`;
  } catch (error) {
    if (searchId !== currentSearch) return;
    const reason = error.name === "AbortError" ? "The live search timed out." : error.message;
    target.innerHTML = `<article class="result-card error-card"><div class="tag">Live search unavailable</div><h3>${esc(reason)}</h3><p>Failure reports could not be checked, so this is not a clear result. Open the source search or try again later.</p><a href="${esc(fallback)}" target="_blank" rel="noopener noreferrer">Open failure reports ↗</a></article>`;
  } finally { clearTimeout(timer); }
}

mode?.addEventListener("change", () => {
  currentSearch++;
  clearResults();
  updateSearchMode();
});
form?.addEventListener("submit", event => {
  event.preventDefault();
  const query = input.value.trim();
  if (!mode?.value) {
    if (searchHelp) searchHelp.textContent = "Choose a filter before searching.";
    mode?.focus();
    return;
  }
  if (mode?.value === "emulators") {
    if (!query) return;
    const normalizedQuery = query.toLowerCase();
    const lookup = emulatorLookups
      .flatMap(entry => entry.aliases.map(alias => ({ entry, alias })))
      .sort((a, b) => b.alias.length - a.alias.length)
      .find(candidate => normalizedQuery.startsWith(`${candidate.alias} `));
    if (!lookup) {
      const console = consoleNames
        .flatMap(entry => entry.aliases.map(alias => ({ entry, alias })))
        .sort((a, b) => b.alias.length - a.alias.length)
        .find(candidate => normalizedQuery.startsWith(`${candidate.alias} `));
      const hasNoList = console || consolesWithoutVerifiedList.some(alias => normalizedQuery.startsWith(`${alias} `));
      if (searchHelp) searchHelp.textContent = hasNoList
        ? "No dedicated emulator compatibility list has been verified for that console yet."
        : "Enter a supported console name or short name, followed by the game ID. For example: PS3 BLUS30443, PS4 CUSA08519, PS2 SLUS-20091, or Xbox 360 58410B16.";
      input.focus();
      return;
    }
    const gameId = query.slice(lookup.alias.length).trim();
    if (!/^[a-z0-9][a-z0-9_-]*$/i.test(gameId)) {
      if (searchHelp) searchHelp.textContent = "Enter one game ID after the console name, for example PS3 BLUS30443 or PS4 CUSA08519.";
      input.focus();
      return;
    }
    if (["xemu", "Xenia Canary"].includes(lookup.entry.name) && !/^[a-f0-9]{8}$/i.test(gameId)) {
      if (searchHelp) searchHelp.textContent = "Enter the 8-character hexadecimal game ID used by this Xbox compatibility list.";
      input.focus();
      return;
    }
    currentSearch++;
    output.hidden = true;
    output.innerHTML = "";
    history.replaceState(null, "", `?mode=emulators&q=${encodeURIComponent(query)}`);
    window.open(lookup.entry.url(gameId), "_blank", "noopener,noreferrer");
    return;
  }
  run(input.value, mode.value);
  history.replaceState(null, "", `?mode=${encodeURIComponent(mode.value)}&q=${encodeURIComponent(query)}`);
});
homeButton?.addEventListener("click", () => {
  window.location.assign(new URL("index.html", document.baseURI).href);
});
document.querySelectorAll(".query").forEach(button => button.addEventListener("click", () => {
  if (!input) return;
  input.value = button.dataset.query || "";
  run(input.value, mode?.value || "games");
}));
if (input?.value && mode?.value && mode.value !== "emulators") run(input.value);
