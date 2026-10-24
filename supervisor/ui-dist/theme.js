// Theme before first paint: the saved choice, else the system setting.
(function () {
  var t = null;
  try { t = localStorage.getItem("ocl-theme"); } catch (e) {}
  var dark = t === "dark" || ((t === null || t === "system") && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
})();
