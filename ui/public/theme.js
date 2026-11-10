// Applies the theme picked in the title bar before the first paint, so a
// page in "Dunkel" never flashes light (or the other way round). A classic
// script from the same origin: the CSP of `nestegg ui` forbids inline
// scripts. Same storage key and values as src/lib/theme.ts.
(function () {
  try {
    var t = window.localStorage.getItem("nestegg.theme");
    if (t === "light" || t === "dark") document.documentElement.setAttribute("data-theme", t);
  } catch (e) {
    // Storage blocked: follow the system.
  }
})();
