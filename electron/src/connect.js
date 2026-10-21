// Connect-page logic: validate-less submit (validation lives in the main
// process via normalizeServerUrl) plus inline error display. The main
// window navigates away to the server on success.
(() => {
  const api = window.huddleConnect;
  if (!api) return; // not running inside the shell

  const form = document.getElementById("form");
  const input = document.getElementById("server");
  const button = document.getElementById("connect");
  const error = document.getElementById("error");

  const urlError = new URLSearchParams(window.location.search).get("error");
  if (urlError) showError(urlError);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    hideError();
    button.disabled = true;
    try {
      const result = await api.submit(input.value.trim());
      if (result && !result.ok) showError(result.error);
    } catch (err) {
      showError(String(err && err.message ? err.message : err));
    } finally {
      button.disabled = false;
    }
  });

  function showError(message) {
    error.textContent = message;
    error.dataset.visible = "";
  }

  function hideError() {
    delete error.dataset.visible;
  }
})();
