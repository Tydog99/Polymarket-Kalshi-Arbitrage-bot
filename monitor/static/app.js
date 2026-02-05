function setActiveNav() {
  const path = window.location.pathname;
  document.querySelectorAll("[data-nav]").forEach((a) => {
    const href = a.getAttribute("href");
    const active =
      (href === "/" && path === "/") ||
      (href !== "/" && path.startsWith(href));
    if (active) a.classList.add("active");
    else a.classList.remove("active");
  });
}

window.addEventListener("DOMContentLoaded", () => {
  setActiveNav();
});

