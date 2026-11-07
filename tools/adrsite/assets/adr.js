// Site behavior for ADR pages:
//  - decision titles copy their full reference (<adr-folder>/<slug>) on click
//  - handbook filters: "only decisions in force" and tag chips (match any).
//    Without JavaScript the filter bar stays hidden and every decision shows.
(function () {
  "use strict";
  var STORE_KEY = "cairn-handbook-in-force-only";

  function storedInForceOnly() {
    try {
      return localStorage.getItem(STORE_KEY) !== "0";
    } catch (e) {
      return true;
    }
  }

  function remember(value) {
    try {
      localStorage.setItem(STORE_KEY, value ? "1" : "0");
    } catch (e) { /* storage unavailable: fine */ }
  }

  function tagsFromUrl() {
    var raw = new URLSearchParams(window.location.search).get("tags");
    return raw ? raw.split(",").filter(Boolean) : [];
  }

  function writeTagsToUrl(tags) {
    var url = new URL(window.location.href);
    if (tags.length) url.searchParams.set("tags", tags.join(","));
    else url.searchParams.delete("tags");
    history.replaceState(null, "", url);
  }

  function setNavHidden(id, hidden) {
    document.querySelectorAll('.md-nav a[href="#' + id + '"]').forEach(function (a) {
      var li = a.closest("li");
      if (li) li.hidden = hidden;
    });
  }

  function init() {
    var bar = document.getElementById("adr-filter");
    if (!bar || bar.dataset.ready) return;
    bar.dataset.ready = "1";
    bar.hidden = false;

    var inForce = document.getElementById("adr-in-force-only");
    var clear = document.getElementById("adr-tag-clear");
    var count = document.getElementById("adr-filter-count");
    var chips = Array.prototype.slice.call(bar.querySelectorAll(".adr-tag-chip"));
    var entries = Array.prototype.slice.call(document.querySelectorAll(".adr-entry"));
    var categories = Array.prototype.slice.call(document.querySelectorAll(".adr-category"));
    var known = chips.map(function (c) { return c.dataset.tag; });
    var selected = tagsFromUrl().filter(function (t) { return known.indexOf(t) !== -1; });

    inForce.checked = storedInForceOnly();

    function apply() {
      var shown = 0;
      entries.forEach(function (el) {
        var tags = (el.dataset.tags || "").split(" ").filter(Boolean);
        var hide =
          (inForce.checked && el.dataset.state !== "active") ||
          (selected.length && !selected.some(function (t) { return tags.indexOf(t) !== -1; }));
        el.hidden = hide;
        if (!hide) shown++;
        var heading = el.querySelector("h3[id]");
        if (heading) setNavHidden(heading.id, hide);
      });
      categories.forEach(function (section) {
        var empty = !section.querySelector(".adr-entry:not([hidden])");
        section.hidden = empty;
        var heading = section.querySelector("h2[id]");
        if (heading) setNavHidden(heading.id, empty);
      });
      chips.forEach(function (chip) {
        chip.setAttribute("aria-pressed", selected.indexOf(chip.dataset.tag) !== -1 ? "true" : "false");
      });
      clear.hidden = selected.length === 0;
      count.textContent = "Showing " + shown + " of " + entries.length + " decisions";
    }

    inForce.addEventListener("change", function () {
      remember(inForce.checked);
      apply();
    });
    chips.forEach(function (chip) {
      chip.addEventListener("click", function () {
        var tag = chip.dataset.tag;
        var i = selected.indexOf(tag);
        if (i === -1) selected.push(tag);
        else selected.splice(i, 1);
        writeTagsToUrl(selected);
        apply();
      });
    });
    clear.addEventListener("click", function () {
      selected = [];
      writeTagsToUrl(selected);
      apply();
    });
    apply();
  }

  // ---- Copy decision references -------------------------------------------

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      var ok = false;
      try { ok = document.execCommand("copy"); } catch (e) { /* unsupported */ }
      document.body.removeChild(area);
      if (ok) resolve(); else reject(new Error("copy failed"));
    });
  }

  function copyRef(el) {
    var ref = el.dataset.ref;
    copyText(ref).then(
      function () { flash(el, "copied"); },
      function () { flash(el, "failed"); }
    );
  }

  function flash(el, state) {
    el.dataset.copyState = state;
    clearTimeout(el._adrCopyTimer);
    el._adrCopyTimer = setTimeout(function () { delete el.dataset.copyState; }, 1500);
  }

  // Delegated, so it also works after instant navigation swaps the page.
  document.addEventListener("click", function (event) {
    var el = event.target.closest && event.target.closest(".md-content .adr-ref");
    if (el) {
      event.preventDefault();
      copyRef(el);
    }
  });
  document.addEventListener("keydown", function (event) {
    if ((event.key === "Enter" || event.key === " ") && event.target.classList &&
        event.target.classList.contains("adr-ref")) {
      event.preventDefault();
      copyRef(event.target);
    }
  });

  // Support both full page loads and Zensical's instant navigation.
  if (window.document$ && typeof window.document$.subscribe === "function") {
    window.document$.subscribe(init);
  } else if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
