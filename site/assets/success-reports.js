(() => {
  const api = String(window.DF_SUCCESS_API || "").replace(/\/+$/, "");
  const form = document.querySelector("#success-report-form");
  const notice = document.querySelector("#success-api-notice");
  const status = document.querySelector("#success-report-status");
  const idField = document.querySelector("#report-game-id");
  if (!form || !status) return;

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
  const logField = form.elements.game_log;
  const dropZone = document.querySelector("#report-drop-zone");
  const logName = document.querySelector("#report-log-name");
  const syncLogName = () => {
    const file = logField?.files?.[0];
    if (!logName) return;
    logName.textContent = file ? file.name : "";
    logName.hidden = !file;
  };
  if (dropZone && logField) {
    const acceptDroppedFile = file => {
      if (!file) return;
      if (file.size > 5 * 1024 * 1024 || !/\.(log|txt)$/i.test(file.name)) {
        setStatus("Choose a .log or .txt file up to 5 MB.", true);
        return;
      }
      const transfer = new DataTransfer();
      transfer.items.add(file);
      logField.files = transfer.files;
      syncLogName();
      setStatus("");
    };
    dropZone.addEventListener("click", () => logField.click());
    dropZone.addEventListener("keydown", event => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        logField.click();
      }
    });
    dropZone.addEventListener("dragover", event => {
      event.preventDefault();
      dropZone.classList.add("is-dragging");
    });
    dropZone.addEventListener("dragleave", () => dropZone.classList.remove("is-dragging"));
    dropZone.addEventListener("drop", event => {
      event.preventDefault();
      dropZone.classList.remove("is-dragging");
      acceptDroppedFile(event.dataTransfer?.files?.[0]);
    });
    logField.addEventListener("change", syncLogName);
  }
  const authHeader = () => ({ Authorization: "Bearer " + session });
  const beginAuthorization = () => {
    const returnTo = location.pathname + location.search;
    location.replace(api + "/auth/start?return_to=" + encodeURIComponent(returnTo));
  };

  if (!api || api.includes("__SUCCESS_API_URL__")) {
    notice.hidden = false;
    setStatus("Online success reporting is temporarily unavailable.", true);
    return;
  }

  if (query.get("login") === "cancelled") {
    setStatus("GitHub authorization was cancelled. Reload this page to try again.", true);
  } else if (!session) {
    setStatus("Connecting to GitHub…");
    beginAuthorization();
  } else {
    fetch(api + "/api/me", { headers: authHeader() })
      .then(response => {
        if (response.ok) return response.json();
        const error = new Error("GitHub could not verify the current session.");
        error.expired = response.status === 401 || response.status === 403;
        throw error;
      })
      .then(data => {
        if (!data?.login) throw new Error("GitHub returned an unreadable session.");
        form.hidden = false;
        setStatus("");
      })
      .catch(error => {
        session = "";
        sessionStorage.removeItem("df-github-session");
        form.hidden = true;
        if (error.expired) {
          setStatus("Your GitHub connection expired. Reconnecting…");
          beginAuthorization();
        } else {
          setStatus("Could not verify the GitHub connection. Refresh this page to try again.", true);
        }
      });
  }

  form.addEventListener("submit", async event => {
    event.preventDefault();
    if (!session) {
      setStatus("The GitHub connection expired. Refresh this page to reconnect.", true);
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
      syncLogName();
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
