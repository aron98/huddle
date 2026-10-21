// Picker-page logic: renders the desktopCapturer sources sent by the main
// process. Escape (or closing the window) cancels and getDisplayMedia
// rejects with NotAllowedError, like Chrome's native dialog. Source names
// come from window titles, so they are always inserted via textContent.
(() => {
  const api = window.huddlePicker;
  if (!api) return; // not running inside the shell

  const grid = document.getElementById("grid");
  const empty = document.getElementById("empty");
  const audioRow = document.getElementById("audio-row");

  api.onSources(({ sources, audioSupported }) => {
    if (audioSupported) audioRow.dataset.supported = "";
    grid.replaceChildren();
    if (sources.length === 0) {
      empty.dataset.visible = "";
      return;
    }
    for (const source of sources) {
      const tile = document.createElement("button");
      tile.type = "button";
      tile.className = "tile";

      const img = document.createElement("img");
      img.src = source.thumbnail;
      img.alt = "";

      const name = document.createElement("div");
      name.className = "name";
      name.textContent = source.name;

      tile.append(img, name);
      tile.addEventListener("click", () => {
        api.select(source.id, document.getElementById("audio").checked);
      });
      grid.append(tile);
    }
    const first = grid.querySelector("button");
    if (first) first.focus();
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") api.cancel();
  });

  api.ready();
})();
