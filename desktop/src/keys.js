// The window's keys (F11, Esc, Ctrl+,). Runs in every page the window shows, the
// dashboard included; the app answers these requests itself (src/gateway.rs).
(() => {
  const fullscreen = (to) =>
    fetch("/__app/fullscreen?to=" + to, { method: "POST", cache: "no-store" }).catch(() => {});
  window.addEventListener(
    "keydown",
    (e) => {
      if (e.key === "F11") {
        e.preventDefault();
        fullscreen("toggle");
      } else if (e.key === "Escape") {
        fullscreen("off");
      } else if (e.key === "," && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        location.href = "/__app/setup";
      }
    },
    true,
  );
})();
