(() => {
  "use strict";

  const container = document.getElementById("format-list");
  const note = document.getElementById("format-note");
  const error = document.getElementById("format-error");

  const textElement = (tag, className, text) => {
    const node = document.createElement(tag);
    node.className = className;
    node.textContent = text;
    return node;
  };

  function render(consoles) {
    const table = document.createElement("table");
    table.className = "format-table";
    const caption = textElement("caption", "sr-only", "Digital Forge supported formats and testing status");
    const head = document.createElement("thead");
    const headerRow = document.createElement("tr");
    for (const title of ["Console", "Supported formats", "Convert type", "Testing status"]) {
      const cell = textElement("th", "", title);
      cell.scope = "col";
      headerRow.append(cell);
    }
    head.append(headerRow);

    const body = document.createElement("tbody");
    for (const consoleInfo of consoles) {
      const row = document.createElement("tr");
      row.append(textElement("th", "format-console", consoleInfo.name));
      row.lastElementChild.scope = "row";
      row.append(textElement("td", "format-value", consoleInfo.formats));
      row.append(textElement("td", "format-value", consoleInfo.convertType));
      row.append(textElement("td", "format-status", consoleInfo.testingStatus));
      body.append(row);
    }

    table.append(caption, head, body);
    container.replaceChildren(table);
  }

  const dataUrl = new URL("supported-formats.json?v=8", document.currentScript.src);
  fetch(dataUrl)
    .then((response) => {
      if (!response.ok) throw new Error("Supported formats request failed");
      return response.json();
    })
    .then((data) => {
      if (!Array.isArray(data.consoles) || !data.consoles.length) throw new Error("Console list is empty");
      if (data.consoles.some((item) => !item || ["name", "formats", "convertType", "testingStatus"].some((key) => typeof item[key] !== "string"))) {
        throw new Error("Console list is invalid");
      }
      container.setAttribute("aria-busy", "false");
      render(data.consoles);
      note.textContent = data.note || "A planned test is not a compatibility result. Emulator tests will use Digital Forge output; passing an emulator test does not confirm original-console compatibility.";
      note.hidden = false;
    })
    .catch(() => {
      container.setAttribute("aria-busy", "false");
      error.hidden = false;
    });
})();
