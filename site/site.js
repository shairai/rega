// Point the visitor at the download for their system, and run the demo video.
// The page works without this script: the video then shows its own controls.
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

// The demo video: plays while on screen (unless the visitor asked for less
// motion), and the step under it that's playing lights up. Click a step to
// jump to it.
(function () {
  var fig = document.querySelector(".demo");
  if (!fig) return;
  var video = fig.querySelector("video");
  var toggle = fig.querySelector(".demo-toggle");
  var steps = Array.prototype.slice.call(fig.querySelectorAll(".demo-steps li"));
  var times = steps.map(function (li) { return parseFloat(li.getAttribute("data-t")) || 0; });
  var userPaused = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  video.removeAttribute("controls");
  toggle.hidden = false;

  function showPaused(paused) {
    fig.classList.toggle("is-paused", paused);
    toggle.setAttribute("aria-label", paused ? "Play the demo" : "Pause the demo");
  }
  function play() {
    var p = video.play();
    if (p && p.catch) p.catch(function () { showPaused(true); });
  }
  function update() {
    var t = video.currentTime, end = video.duration || 0, current = 0;
    times.forEach(function (start, i) { if (t >= start) current = i; });
    steps.forEach(function (li, i) {
      var from = times[i], to = i + 1 < times.length ? times[i + 1] : end;
      var p = i < current ? 100 : i > current ? 0 : Math.max(0, Math.min(100, (t - from) / Math.max(0.01, to - from) * 100));
      li.classList.toggle("is-current", i === current);
      li.style.setProperty("--p", p + "%");
    });
    if (!video.paused) window.requestAnimationFrame(update);
  }

  video.addEventListener("play", function () { showPaused(false); update(); });
  video.addEventListener("pause", function () { showPaused(true); });
  video.addEventListener("seeked", update);
  toggle.addEventListener("click", function () {
    userPaused = !video.paused;
    if (userPaused) video.pause(); else play();
  });
  steps.forEach(function (li, i) {
    li.setAttribute("tabindex", "0");
    li.setAttribute("role", "button");
    function go() { userPaused = false; video.currentTime = times[i]; play(); }
    li.addEventListener("click", go);
    li.addEventListener("keydown", function (e) {
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); go(); }
    });
  });

  showPaused(true);
  update();
  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting && !userPaused) play();
        else if (!e.isIntersecting) video.pause();
      });
    }, { threshold: 0.3 }).observe(video);
  } else if (!userPaused) {
    play();
  }
})();
