// Point the visitor at the download for their system. Everything else on the
// site is static HTML, so the page works the same without this script.
(function () {
  var ua = navigator.userAgent || "";
  var platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  var os = /Win/i.test(platform) || /Windows/i.test(ua) ? "windows"
    : /Mac/i.test(platform) || /Macintosh/i.test(ua) ? "mac"
    : null;
  // iPhone and iPad report "Mac"-like strings too; they can't run Rega.
  if (/iPhone|iPad|iPod|Android/i.test(ua) || (os === "mac" && navigator.maxTouchPoints > 1)) os = null;
  if (!os) return;

  var card = document.querySelector('.download[data-os="' + os + '"]');
  if (card) card.classList.add("is-you");

  var label = os === "mac" ? "Download for Mac" : "Download for Windows";
  document.querySelectorAll("[data-os-cta]").forEach(function (a) {
    a.setAttribute("href", "/download/" + os);
    var text = a.querySelector("span");
    if (text) text.textContent = label;
  });
})();
