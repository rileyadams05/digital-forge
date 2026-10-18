(() => {
  const api = String(window.DF_SUCCESS_API || "").replace(/\/+$/, "");
  const form = document.querySelector("#success-report-form");
  const signIn = document.querySelector("#github-sign-in");
  const signOut = document.querySelector("#github-sign-out");
  const account = document.querySelector("#github-account");
  const notice = document.querySelector("#success-api-notice");
  const status = document.querySelector("#success-report-status");
  const idField = document.querySelector("#report-game-id");
  if (!form || !signIn || !status) return;

  let session = sessionStorage.getItem("df-github-session") || "";
  const fragment = new URLSearchParams(location.hash.slice(1));
  const returnedSession = fragment.get("github_session");
  if (returnedSession) {
    session = returnedSession;
    sessionStorage.setItem("df-github-session", session);
    history.replaceState(null, "", location.pathname + location.search);
  }
  const query = new URLSearchParams(location.search);
  if (query.has("game") && idField) idField.value = query.get("game").slice(0, 32);

  const setStatus = (message, error = false) => {
    status.textContent = message;
    status.classList.toggle("error-message", error);
  };
  const setSignedIn = login => {
    account.hidden = !login;
    account.textContent = login ? "Signed in as @" + login : "";
    signIn.hidden = Boolean(login);
    signOut.hidden = !login;
    form.hidden = !login;
  };
  const authHeader = () => ({ Authorization: "Bearer " + session });

  if (!api || api.includes("__SUCCESS_API_URL__")) {
    notice.hidden = false;
    signIn.disabled = true;
    signIn.setAttribute("aria-disabled", "true");
    setStatus("Success reporting is not connected yet.");
    return;
  }

  signIn.addEventListener("click", () => {
    const returnTo = location.pathname + location.search;
    location.href = api + "/auth/start?return_to=" + encodeURIComponent(returnTo);
  });
  signOut.addEventListener("click", () => {
    session = "";
    sessionStorage.removeItem("df-github-session");
    setSignedIn("");
    setStatus("Signed out of GitHub.");
  });
  if (session) {
    fetch(api + "/api/me", { headers: authHeader() })
      .then(response => response.ok ? response.json() : Promise.reject())
      .then(data => setSignedIn(data.login))
      .catch(() => {
        session = "";
        sessionStorage.removeItem("df-github-session");
        setSignedIn("");
        setStatus("Your GitHub sign-in expired. Sign in again to submit.");
      });
  } else setSignedIn("");

  form.addEventListener("submit", async event => {
    event.preventDefault();
    if (!session) {
      setStatus("Sign in with GitHub before submitting.", true);
      return;
    }
    const file = form.elements.game_log.files?.[0];
    if (!file || file.size > 5 * 1024 * 1024 || !/\.(log|txt)$/i.test(file.name)) {
      setStatus("Choose a .log or .txt file up to 5 MB.", true);
      return;
    }
    const submitButton = form.querySelector('[type="submit"]');
    submitButton.disabled = true;
    setStatus("Preparing your review request…");
    try {
      const response = await fetch(api + "/api/success-reports", { method: "POST", headers: authHeader(), body: new FormData(form) });
      const result = await response.json();
      if (!response.ok) throw new Error(result.error || "The report could not be prepared.");
      form.reset();
      if (idField && query.has("game")) idField.value = query.get("game").slice(0, 32);
      setStatus("Your report is ready for review. Open the pull request to see its status.");
      const link = document.createElement("a");
      link.href = result.pull_request;
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      link.textContent = "Open pull request #" + result.number;
      status.append(" ", link);
    } catch (error) {
      setStatus(error.message || "The report could not be submitted. Try again later.", true);
    } finally {
      submitButton.disabled = false;
    }
  });
})();
