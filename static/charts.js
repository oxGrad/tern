// Initialises every [data-chart] element as an ECharts instance, on load and
// after each htmx swap. The server writes colors as @tokens; they resolve here
// per theme (validated reference palette, light and dark steps).
(function () {
  const light = {
    s0: "#898781", s1: "#2a78d6", s2: "#eb6834", s3: "#1baf7a", s4: "#eda100",
    s5: "#e87ba4", s6: "#008300", s7: "#6250d6", s8: "#e34948",
    seq0: "#cde2fb", seq1: "#104281", surface: "#fcfcfb", ink: "#0b0b0b",
    ink2: "#52514e", muted: "#898781", grid: "#e1e0d9", band: "rgba(137,135,129,0.12)",
  };
  const dark = {
    s0: "#898781", s1: "#3987e5", s2: "#d95926", s3: "#199e70", s4: "#c98500",
    s5: "#d55181", s6: "#008300", s7: "#9085e9", s8: "#e66767",
    seq0: "#184f95", seq1: "#cde2fb", surface: "#1a1a19", ink: "#f0efec",
    ink2: "#c3c2b7", muted: "#898781", grid: "#2c2c2a", band: "rgba(137,135,129,0.18)",
  };
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  const tokens = () => (mq.matches ? dark : light);

  function resolve(json) {
    const t = tokens();
    return JSON.parse(json.replace(/"@(\w+)"/g, (m, k) => (k in t ? JSON.stringify(t[k]) : m)));
  }

  function axis(t) {
    return {
      axisLine: { lineStyle: { color: t.grid } },
      axisTick: { show: false },
      axisLabel: { color: t.muted },
      splitLine: { lineStyle: { color: t.grid } },
      nameTextStyle: { color: t.muted },
    };
  }

  function build(el) {
    const t = tokens();
    const opt = resolve(el.dataset.chart);
    opt.backgroundColor = "transparent";
    opt.textStyle = { color: t.ink2, fontFamily: "inherit" };
    if (opt.legend) opt.legend.textStyle = { color: t.ink2 };
    for (const k of ["xAxis", "yAxis"]) if (opt[k]) opt[k] = Object.assign(axis(t), opt[k]);
    opt.tooltip = Object.assign({ backgroundColor: t.surface, borderColor: t.grid, textStyle: { color: t.ink } }, opt.tooltip);
    if (opt.visualMap) opt.visualMap.textStyle = { color: t.muted };
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

  function rebuildAll() {
    document.querySelectorAll("[data-chart]").forEach((el) => {
      if (el._chart) { el._chart.dispose(); el._chart = null; }
      build(el);
    });
  }

  document.addEventListener("htmx:beforeSwap", (e) => {
    e.detail.target.querySelectorAll("[data-chart]").forEach((el) => el._chart && el._chart.dispose());
  });
  document.addEventListener("htmx:afterSettle", () => init(document));
  mq.addEventListener("change", rebuildAll);
  window.addEventListener("resize", () => document.querySelectorAll("[data-chart]").forEach((el) => el._chart && el._chart.resize()));
  init(document);
})();
