// Initialises every [data-chart] element as an ECharts instance, on load and
// after each htmx swap. The server writes colors as @tokens; they resolve to
// the light or the dark set depending on the band the chart sits on.
(function () {
  const light = {
    s0: "#7a7a7a", s1: "#2a78d6", s2: "#eb6834", s3: "#1baf7a", s4: "#eda100",
    s5: "#e87ba4", s6: "#008300", s7: "#6250d6", s8: "#e34948",
    seq0: "#e8f0fb", seq1: "#104281", surface: "#ffffff", ink: "#1d1d1f",
    ink2: "#333333", grid: "#f0f0f0", axis: "#e0e0e0",
    band: "rgba(0, 0, 0, 0.04)", sel: "rgba(0, 102, 204, 0.08)",
  };
  const dark = {
    s0: "#7a7a7a", s1: "#3987e5", s2: "#d95926", s3: "#199e70", s4: "#c98500",
    s5: "#d55181", s6: "#3fa03f", s7: "#9085e9", s8: "#e66767",
    seq0: "#184f95", seq1: "#cde2fb", surface: "#272729", ink: "#ffffff",
    ink2: "#cccccc", grid: "#3a3a3c", axis: "#3a3a3c",
    band: "rgba(255, 255, 255, 0.06)", sel: "rgba(41, 151, 255, 0.14)",
  };
  const font = getComputedStyle(document.body).fontFamily;

  function tokensFor(el) {
    return el.closest(".band-dark") ? dark : light;
  }

  function resolve(json, t) {
    return JSON.parse(json.replace(/"@(\w+)"/g, (m, k) => (k in t ? JSON.stringify(t[k]) : m)));
  }

  function axis(t) {
    return {
      axisLine: { lineStyle: { color: t.axis } },
      axisTick: { show: false },
      axisLabel: { color: t.ink2, fontSize: 12 },
      splitLine: { lineStyle: { color: t.grid } },
      nameTextStyle: { color: t.ink2, fontSize: 13 },
    };
  }

  function build(el) {
    const t = tokensFor(el);
    const opt = resolve(el.dataset.chart, t);
    opt.backgroundColor = "transparent";
    opt.textStyle = { color: t.ink2, fontFamily: font };
    if (opt.legend) opt.legend.textStyle = { color: t.ink2, fontSize: 14 };
    for (const k of ["xAxis", "yAxis"]) if (opt[k]) opt[k] = Object.assign(axis(t), opt[k]);
    opt.tooltip = Object.assign(
      { backgroundColor: t.surface, borderColor: t.axis, textStyle: { color: t.ink, fontFamily: font } },
      opt.tooltip
    );
    if (opt.visualMap) opt.visualMap.textStyle = { color: t.ink2 };
    const c = echarts.init(el);
    c.setOption(opt);
    if (el.dataset.click) {
      el.style.cursor = "pointer";
      c.on("click", (p) => {
        const url = el.dataset.click
          .replace("{name}", encodeURIComponent(p.name || ""))
          .replace("{series}", encodeURIComponent(p.seriesName || ""));
        htmx.ajax("GET", url, { target: el.dataset.target, swap: "outerHTML" });
      });
    }
    el._chart = c;
  }

  function init(root) {
    root.querySelectorAll("[data-chart]").forEach((el) => { if (!el._chart) build(el); });
  }

  document.addEventListener("htmx:beforeSwap", (e) => {
    e.detail.target.querySelectorAll("[data-chart]").forEach((el) => el._chart && el._chart.dispose());
  });
  document.addEventListener("htmx:afterSettle", () => init(document));
  window.addEventListener("resize", () =>
    document.querySelectorAll("[data-chart]").forEach((el) => el._chart && el._chart.resize())
  );
  init(document);
})();
