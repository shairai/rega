// Cookie consent for the site's analytics: Google Tag Manager, which runs
// Google Analytics. Nothing from Google loads until the visitor clicks
// Accept. The choice is remembered in this browser and can be changed with
// "Cookie settings" in the footer. Download clicks are sent to Tag Manager as
// a `rega_download` event (see site/README.md for the container setup).
(function () {
  var GTM_ID = "GTM-5Q9HZX6Z";
  var KEY = "rega-consent";

  window.dataLayer = window.dataLayer || [];
  function gtag() { window.dataLayer.push(arguments); }
  // Consent Mode: everything off until the visitor says otherwise.
  gtag("consent", "default", {
    ad_storage: "denied",
    ad_user_data: "denied",
    ad_personalization: "denied",
    analytics_storage: "denied"
  });

  function stored() {
    try { return window.localStorage.getItem(KEY); } catch (e) { return null; }
  }
  function remember(choice) {
    try { window.localStorage.setItem(KEY, choice); } catch (e) { /* private mode: ask again next time */ }
  }

  var loaded = false;
  function load() {
    gtag("consent", "update", { analytics_storage: "granted" });
    if (loaded) return;
    loaded = true;
    window.dataLayer.push({ "gtm.start": new Date().getTime(), event: "gtm.js" });
    var s = document.createElement("script");
    s.async = true;
    s.src = "https://www.googletagmanager.com/gtm.js?id=" + GTM_ID;
    document.head.appendChild(s);
  }

  // Turn analytics off and drop the cookies an earlier Accept left.
  function forget() {
    gtag("consent", "update", { analytics_storage: "denied" });
    document.cookie.split(";").forEach(function (c) {
      var name = c.split("=")[0].trim();
      if (name.indexOf("_ga") !== 0 && name.indexOf("_gid") !== 0) return;
      var gone = name + "=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/";
      document.cookie = gone;
      document.cookie = gone + "; domain=" + location.hostname;
    });
  }

  function choose(choice) {
    remember(choice);
    if (choice === "granted") load(); else forget();
    hide();
  }

  var banner = null;
  function show() {
    if (banner) return;
    banner = document.createElement("div");
    banner.className = "consent";
    banner.setAttribute("role", "dialog");
    banner.setAttribute("aria-label", "Cookies");
    banner.innerHTML =
      '<p><b>Cookies?</b> We\'d like to count visits and downloads with Google Analytics. ' +
      'Nothing from Google loads unless you accept. <a href="/privacy#website">Details</a></p>' +
      '<div class="consent-actions">' +
      '<button type="button" class="btn btn-small" data-choice="denied">Decline</button>' +
      '<button type="button" class="btn btn-small" data-choice="granted">Accept</button>' +
      "</div>";
    banner.addEventListener("click", function (e) {
      var b = e.target.closest && e.target.closest("[data-choice]");
      if (b) choose(b.getAttribute("data-choice"));
    });
    document.body.appendChild(banner);
  }
  function hide() {
    if (banner) banner.remove();
    banner = null;
  }

  if (stored() === "granted") load();

  document.addEventListener("DOMContentLoaded", function () {
    if (!stored()) show();
    document.querySelectorAll("[data-consent-open]").forEach(function (a) {
      a.addEventListener("click", function (e) { e.preventDefault(); show(); });
    });
  });

  // Count download clicks (only sent anywhere once Tag Manager is loaded).
  document.addEventListener("click", function (e) {
    var a = e.target.closest && e.target.closest('a[href^="/download/"]');
    if (!a || !loaded) return;
    var any = document.querySelector("[data-version]");
    window.dataLayer.push({
      event: "rega_download",
      download_os: a.getAttribute("href").split("/")[2],
      download_version: a.getAttribute("data-version") || (any && any.getAttribute("data-version")) || "",
      download_placement: a.hasAttribute("data-os-cta") ? "hero" : "download-card",
      download_page: location.pathname
    });
  });
})();
