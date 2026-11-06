// Panel del colector de telemetría de AetherLynx.
// Sin dependencias. Todo lo que viene de la telemetría se inserta como
// texto (textContent), nunca como HTML.
"use strict";

const TOKEN_KEY = "aetherlynx.telemetry.token";
const SEVERITY = [
  { min: 0, label: "Todos" },
  { min: 9, label: "Info o más" },
  { min: 13, label: "Aviso o más" },
  { min: 17, label: "Error o más" },
];
const COLORS = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6"];

const state = {
  token: null,
  tab: "resumen",
  win: "1h",
  timer: null,
  metric: null,
  metricMode: "auto",
  traceFilter: { service: "", errors: false },
  logFilter: { service: "", severity: 0, q: "" },
};

// ---------------------------------------------------------------- utilidades

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    // Por CSSOM: la CSP (style-src 'self') bloquea los atributos style.
    else if (k === "style") {
      for (const decl of String(v).split(";")) {
        const i = decl.indexOf(":");
        if (i > 0) el.style.setProperty(decl.slice(0, i).trim(), decl.slice(i + 1).trim());
      }
    }
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const SVG = "http://www.w3.org/2000/svg";
function s(tag, attrs, ...children) {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, String(v));
  for (const c of children.flat()) if (c) el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  return el;
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

function fmtTime(ns) {
  const d = new Date(ns / 1e6);
  return d.toLocaleTimeString("es", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
function fmtDateTime(ns) {
  return new Date(ns / 1e6).toLocaleString("es", { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
function fmtAgo(ns) {
  const s = Math.max(0, Math.round((Date.now() * 1e6 - ns) / 1e9));
  if (s < 60) return `hace ${s} s`;
  if (s < 3600) return `hace ${Math.round(s / 60)} min`;
  return `hace ${Math.round(s / 3600)} h`;
}
function fmtMs(ms) {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
  if (ms < 1000) return `${ms.toFixed(ms < 10 ? 1 : 0)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}
function fmtNum(v, unit) {
  if (v === null || v === undefined || !isFinite(v)) return "—";
  if (unit === "s") return fmtMs(v * 1000);
  const a = Math.abs(v);
  if (Number.isInteger(v) && a < 1e4) return unit ? `${v} ${unit}` : String(v);
  const n = a >= 1e6 ? (v / 1e6).toFixed(1) + " M" : a >= 1e3 ? (v / 1e3).toFixed(1) + " k" : a >= 100 ? v.toFixed(0) : a >= 1 ? v.toFixed(2) : v.toPrecision(2);
  return unit ? `${n} ${unit}` : n;
}
function severityClass(n) {
  return n >= 21 ? "fatal" : n >= 17 ? "error" : n >= 13 ? "warn" : n >= 9 ? "info" : "";
}
function attrLabel(a) {
  if (!a || typeof a !== "object") return "";
  const parts = Object.entries(a).map(([k, v]) => `${k}=${v}`);
  return parts.join(", ");
}

// --------------------------------------------------------------------- API

class AuthError extends Error {}

async function api(path, params) {
  const url = new URL(path, location.origin);
  for (const [k, v] of Object.entries(params || {})) {
    if (v !== "" && v !== null && v !== undefined && v !== false) url.searchParams.set(k, v);
  }
  const res = await fetch(url, { headers: { Authorization: `Bearer ${state.token}` }, cache: "no-store" });
  if (res.status === 401) throw new AuthError("token rechazado");
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
  return body;
}

// --------------------------------------------------------------- gráficos

function niceTicks(min, max, count, integers) {
  if (min === max) { min -= 1; max += 1; }
  const span = max - min;
  const step0 = span / count;
  const mag = Math.pow(10, Math.floor(Math.log10(step0)));
  let step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((st) => span / st <= count) || 10 * mag;
  // Para conteos (agentes, réplicas) no tiene sentido marcar 0,25.
  if (integers) step = Math.max(1, Math.ceil(step));
  const lo = Math.floor(min / step) * step;
  const hi = Math.ceil(max / step) * step;
  const ticks = [];
  for (let v = lo; v <= hi + step / 2; v += step) ticks.push(+v.toPrecision(12));
  return ticks;
}

/** Ancho disponible para un gráfico: se dibuja a escala 1:1 para que el
 *  texto mida siempre lo mismo, en pantallas anchas y en el móvil. */
function chartWidth(kind) {
  const view = document.getElementById("view");
  const w = (view && view.clientWidth) || 960;
  const pad = 2 * 14 + 2;
  if (kind === "wide") {
    const listW = window.innerWidth > 760 ? 300 + 16 : 0;
    return Math.max(280, Math.min(1100, w - listW - pad));
  }
  const cols = Math.max(1, Math.floor((w + 16) / (420 + 16)));
  return Math.max(260, Math.floor((w - (cols - 1) * 16) / cols) - pad);
}

/** Gráfico de líneas. series: [{label, points: [[ms, v], ...]}].
 *  opts.step: dibuja escalones (gauges: el valor se mantiene hasta el siguiente punto). */
function lineChart(series, unit, opts) {
  const o = Object.assign({ step: false, width: chartWidth() }, opts || {});
  const W = o.width, H = 200, L = 56, R = 12, T = 10, B = 26;
  const wrap = h("div", {});
  const pts = series.flatMap((x) => x.points);
  if (pts.length === 0) {
    wrap.append(h("p", { class: "empty" }, "Sin datos en esta ventana."));
    return wrap;
  }
  const xs = pts.map((p) => p[0]), ys = pts.map((p) => p[1]);
  let x0 = Math.min(...xs), x1 = Math.max(...xs);
  if (x0 === x1) { x0 -= 30000; x1 += 30000; }
  const integers = ys.every((v) => Number.isInteger(v));
  const yt = niceTicks(Math.min(0, ...ys), Math.max(...ys), 4, integers);
  const y0 = yt[0], y1 = yt[yt.length - 1];
  const sx = (x) => L + ((x - x0) / (x1 - x0)) * (W - L - R);
  const sy = (y) => T + (1 - (y - y0) / (y1 - y0 || 1)) * (H - T - B);

  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "chart", role: "img", "aria-label": "Gráfico de serie temporal" });
  const grid = s("g", { class: "grid" });
  for (const v of yt) {
    grid.append(s("line", { x1: L, x2: W - R, y1: sy(v), y2: sy(v) }));
    svg.append(s("text", { x: L - 6, y: sy(v) + 4, "text-anchor": "end" }, fmtNum(v, unit)));
  }
  svg.append(grid);
  const nx = W < 420 ? 3 : 5;
  for (let i = 0; i <= nx; i++) {
    const t = x0 + ((x1 - x0) * i) / nx;
    const d = new Date(t);
    const label = d.toLocaleTimeString("es", { hour: "2-digit", minute: "2-digit" });
    svg.append(s("text", { x: sx(t), y: H - 6, "text-anchor": i === 0 ? "start" : i === nx ? "end" : "middle" }, label));
  }
  svg.append(s("line", { class: "axis", x1: L, x2: W - R, y1: H - B, y2: H - B }));

  series.forEach((ser, i) => {
    const color = cssVar(COLORS[i % COLORS.length]);
    if (ser.points.length === 1) {
      const [x, y] = ser.points[0];
      svg.append(s("circle", { cx: sx(x), cy: sy(y), r: 3, fill: color }));
      return;
    }
    const d = ser.points.map((p, j) => {
      const x = sx(p[0]).toFixed(1), y = sy(p[1]).toFixed(1);
      if (!j) return `M${x},${y}`;
      return o.step ? `H${x}V${y}` : `L${x},${y}`;
    }).join("");
    svg.append(s("path", { d, fill: "none", stroke: color, "stroke-width": 1.8, "stroke-linejoin": "round" }));
    const last = ser.points[ser.points.length - 1];
    svg.append(s("circle", { cx: sx(last[0]), cy: sy(last[1]), r: 2.8, fill: color }));
  });

  const hover = s("line", { class: "hover-line", y1: T, y2: H - B, x1: L, x2: L, visibility: "hidden" });
  svg.append(hover);
  const tip = h("p", { class: "tip" }, " ");
  svg.addEventListener("pointermove", (ev) => {
    const r = svg.getBoundingClientRect();
    const x = x0 + ((((ev.clientX - r.left) / r.width) * W - L) / (W - L - R)) * (x1 - x0);
    const parts = series.map((ser) => {
      let best = null;
      for (const p of ser.points) if (!best || Math.abs(p[0] - x) < Math.abs(best[0] - x)) best = p;
      return best ? { label: ser.label, p: best } : null;
    }).filter(Boolean);
    if (!parts.length) return;
    const at = parts[0].p[0];
    hover.setAttribute("x1", sx(at)); hover.setAttribute("x2", sx(at)); hover.setAttribute("visibility", "visible");
    tip.textContent = `${new Date(at).toLocaleTimeString("es")} · ` + parts.map((q) => `${q.label || "valor"}: ${fmtNum(q.p[1], unit)}`).join(" · ");
  });
  svg.addEventListener("pointerleave", () => { hover.setAttribute("visibility", "hidden"); tip.textContent = " "; });

  wrap.append(svg, tip);
  if (series.length > 1 || (series[0] && series[0].label)) {
    wrap.append(h("div", { class: "legend" }, series.map((ser, i) => h("span", { style: `--c: var(${COLORS[i % COLORS.length]})` }, ser.label || "valor"))));
  }
  return wrap;
}

function unitFor(resp, mode) {
  const kind = resp.kind;
  const m = mode === "auto" ? (kind === "histogram" ? "p95" : kind === "sum" ? "rate" : "value") : mode;
  if (m === "rate") return "/s";
  if (kind === "histogram" && m !== "value") return resp.unit || "";
  return resp.unit === "s" ? "s" : resp.unit || "";
}

/** Tarjeta con una o varias métricas. `name` puede ser una lista de
 *  {name, label} para superponer series (p. ej. réplicas pedidas y listas). */
async function metricCard(title, name, opts) {
  const o = Object.assign({ mode: "auto", service: "", width: chartWidth() }, opts || {});
  const parts = Array.isArray(name) ? name : [{ name, label: o.label }];
  const card = h("div", { class: "card" }, h("div", { class: "card-head" }, h("h3", {}, title), h("span", { class: "muted mono" }, parts.map((p) => p.name).join(" · "))));
  try {
    const resps = await Promise.all(parts.map((p) => api("/api/metrics/series", { name: p.name, mode: o.mode, since: state.win, service: o.service })));
    const series = resps.flatMap((r, i) => r.series.map((x) => ({
      label: typeof parts[i].label === "function" ? parts[i].label(x) : (parts[i].label || attrLabel(x.attributes) || x.service),
      points: x.points,
    })));
    const r0 = resps[0];
    const gauge = r0.kind === "gauge" || o.mode === "value";
    card.append(lineChart(series, unitFor(r0, o.mode), { step: gauge, width: o.width }));
  } catch (e) {
    if (e instanceof AuthError) throw e;
    card.append(h("p", { class: "error" }, e.message));
  }
  return card;
}

// ------------------------------------------------------------------ vistas

async function viewResumen(root) {
  const [alerts, svcs, status] = await Promise.all([api("/api/alerts"), api("/api/services", { since: state.win }), api("/api/status")]);

  const alertSec = h("section", {}, h("h2", {}, "Alertas activas"));
  if (alerts.active.length === 0) {
    alertSec.append(h("div", { class: "ok-banner" }, "Sin alertas activas."));
  } else {
    alertSec.append(h("div", { class: "alerts" }, alerts.active.map((a) =>
      h("div", { class: `alert ${a.severity}` },
        h("span", { class: `chip sev ${a.severity}` }, a.severity === "critical" ? "Crítica" : "Aviso"),
        h("div", { class: "what" }, `${a.rule} · ${a.service}: ${a.summary}`),
        h("div", { class: "hint" }, a.hint),
        h("div", { class: "meta" }, `Activa desde ${fmtDateTime(a.since_ns)} (${fmtAgo(a.since_ns)})`)))));
  }

  const now = Date.now() * 1e6;
  const svcSec = h("section", {}, h("h2", {}, "Servicios"));
  if (svcs.services.length === 0) {
    svcSec.append(h("p", { class: "empty" }, "Aún no llegó telemetría. Configure OTEL_EXPORTER_OTLP_ENDPOINT en cada componente para que apunte a este colector."));
  } else {
    svcSec.append(h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, ["Servicio", "Última señal", "Operaciones", "Errores", "p50", "p95"].map((t, i) => h("th", { class: i > 1 ? "num" : null }, t)))),
      h("tbody", {}, svcs.services.map((x) => {
        const stale = now - x.last_seen_ns > 5 * 60e9;
        const errPct = x.requests ? (100 * x.errors) / x.requests : 0;
        return h("tr", { class: "click", onclick: () => { state.traceFilter.service = x.service; location.hash = "#trazas"; } },
          h("td", {}, h("span", { class: `dot ${stale ? "stale" : ""}`, title: stale ? "Sin señal reciente" : "Activo" }), x.service),
          h("td", {}, fmtAgo(x.last_seen_ns)),
          h("td", { class: "num" }, x.requests),
          h("td", { class: "num" }, x.requests ? `${errPct.toFixed(1)} %` : "—"),
          h("td", { class: "num" }, fmtMs(x.p50_ms)),
          h("td", { class: "num" }, fmtMs(x.p95_ms)));
      })))));
  }

  const charts = h("section", {}, h("h2", {}, "Indicadores clave"));
  const grid = h("div", { class: "grid" });
  charts.append(grid);
  const cards = await Promise.all([
    metricCard("Agentes por fase", "aetherlynx.agents", { mode: "value", label: (x) => x.attributes.phase }),
    metricCard("Réplicas pedidas y listas", [
      { name: "aetherlynx.replicas.desired", label: "pedidas" },
      { name: "aetherlynx.replicas.ready", label: "listas" },
    ], { mode: "value" }),
    metricCard("Duración del ciclo de reconciliación (p95)", "aetherlynx.reconcile.duration", { mode: "p95" }),
    metricCard("Llamadas al LLM por resultado", "aetherlynx.agent.llm.requests", { mode: "rate", label: (x) => x.attributes.result }),
    metricCard("Latencia del LLM (p95)", "aetherlynx.agent.llm.duration", { mode: "p95" }),
    metricCard("Logins por resultado", "aetherlynx.auth.logins", { mode: "rate", label: (x) => x.attributes.result }),
  ]);
  grid.append(...cards);

  const st = status;
  const collector = h("section", {}, h("h2", {}, "Colector"),
    h("div", { class: "stats" },
      h("span", {}, "Spans guardados ", h("b", {}, st.stored.spans)),
      h("span", {}, "Logs ", h("b", {}, st.stored.logs)),
      h("span", {}, "Puntos de métricas ", h("b", {}, st.stored.metric_points)),
      h("span", {}, "Peticiones rechazadas ", h("b", {}, st.ingested.rejected_requests)),
      st.ingested.rejected_services ? h("span", {}, "Servicios descartados ", h("b", {}, st.ingested.rejected_services)) : null,
      st.ingested.shed_rows ? h("span", {}, "Filas descartadas (base llena) ", h("b", {}, st.ingested.shed_rows)) : null,
      h("span", {}, "Retención ", h("b", {}, `${Math.round(st.retention_s / 3600)} h`)),
      h("span", {}, "Versión ", h("b", {}, st.version))));

  const hist = h("section", {}, h("h2", {}, "Cambios recientes de alertas"));
  if (alerts.history.length === 0) hist.append(h("p", { class: "muted" }, "Sin cambios desde que arrancó el colector."));
  else hist.append(h("div", { class: "table-wrap" }, h("table", {}, h("tbody", {}, alerts.history.slice(0, 15).map((e) =>
    h("tr", {}, h("td", {}, fmtDateTime(e.ts_ns)), h("td", {}, h("span", { class: `chip ${e.state === "firing" ? e.severity : "ok"}` }, e.state === "firing" ? "activa" : "resuelta")),
      h("td", {}, `${e.rule} · ${e.service}`), h("td", { class: "muted" }, e.summary)))))));

  root.append(alertSec, svcSec, charts, collector, hist);
}

async function serviceOptions(selected) {
  const svcs = await api("/api/services", { since: "24h" });
  return [h("option", { value: "" }, "Todos los servicios"), ...svcs.services.map((x) => h("option", { value: x.service, selected: x.service === selected }, x.service))];
}

async function viewTrazas(root) {
  const f = state.traceFilter;
  const svcSel = h("select", { "aria-label": "Servicio", onchange: (e) => { f.service = e.target.value; render(); } }, await serviceOptions(f.service));
  const errChk = h("input", { type: "checkbox", id: "only-errors", checked: f.errors, onchange: (e) => { f.errors = e.target.checked; render(); } });
  root.append(h("section", {}, h("div", { class: "row" }, h("h2", {}, "Operaciones recientes"), svcSel, h("label", { class: "toggle", for: "only-errors" }, errChk, "Solo con error"))));

  const r = await api("/api/traces", { since: state.win, service: f.service, errors: f.errors, limit: 200 });
  if (r.traces.length === 0) {
    root.append(h("p", { class: "empty" }, "No hay trazas que coincidan en esta ventana."));
    return;
  }
  const maxDur = Math.max(...r.traces.map((t) => t.duration_ns));
  root.append(h("div", { class: "table-wrap" }, h("table", {},
    h("thead", {}, h("tr", {}, ["Hora", "Servicio", "Operación", "Duración", "", "Estado"].map((t) => h("th", {}, t)))),
    h("tbody", {}, r.traces.map((t) => h("tr", { class: "click", onclick: () => { location.hash = `#traza/${t.trace_id}`; } },
      h("td", { class: "mono" }, fmtTime(t.start_ns)),
      h("td", {}, t.service),
      h("td", {}, t.name),
      h("td", { class: "num" }, fmtMs(t.duration_ns / 1e6)),
      h("td", { style: "width: 22%" }, h("div", { class: `bar ${t.status_code === 2 ? "err" : ""}`, style: `width: ${Math.max(1, (100 * t.duration_ns) / (maxDur || 1))}%` })),
      h("td", {}, t.status_code === 2 ? h("span", { class: "chip error", title: t.status_message }, "error") : h("span", { class: "chip" }, "ok"))))))));
}

async function viewTraza(root, id) {
  root.append(h("div", { class: "row" }, h("a", { href: "#trazas" }, "← Trazas"), h("h2", {}, "Traza"), h("span", { class: "mono muted" }, id)));
  const r = await api(`/api/traces/${encodeURIComponent(id)}`);
  const spans = r.spans;
  const byId = new Map(spans.map((x) => [x.span_id, x]));
  const children = new Map();
  for (const x of spans) {
    const p = byId.has(x.parent_span_id) ? x.parent_span_id : "";
    if (!children.has(p)) children.set(p, []);
    children.get(p).push(x);
  }
  const ordered = [];
  const walk = (parent, depth) => {
    for (const c of (children.get(parent) || []).sort((a, b) => a.start_ns - b.start_ns)) {
      ordered.push({ span: c, depth });
      walk(c.span_id, depth + 1);
    }
  };
  walk("", 0);
  const t0 = Math.min(...spans.map((x) => x.start_ns));
  const t1 = Math.max(...spans.map((x) => x.end_ns));
  const total = Math.max(1, t1 - t0);
  const services = [...new Set(spans.map((x) => x.service))];
  const color = (svc) => `var(${COLORS[services.indexOf(svc) % COLORS.length]})`;
  const errors = spans.filter((x) => x.status_code === 2).length;

  root.append(h("div", { class: "stats" },
    h("span", {}, "Inicio ", h("b", {}, fmtDateTime(t0))),
    h("span", {}, "Duración ", h("b", {}, fmtMs(total / 1e6))),
    h("span", {}, "Spans ", h("b", {}, spans.length)),
    h("span", {}, "Con error ", h("b", {}, errors)),
    h("span", {}, "Servicios ", h("b", {}, services.join(", ")))));

  const detail = h("div", { class: "card" }, h("p", { class: "muted" }, "Seleccione un span para ver sus atributos."));
  const showDetail = (x, row) => {
    for (const el of wf.querySelectorAll(".wf-row.sel")) el.classList.remove("sel");
    row.classList.add("sel");
    detail.replaceChildren(...[
      h("div", { class: "card-head" }, h("h3", {}, x.name), h("span", { class: "muted" }, `${x.service} · ${fmtMs(x.duration_ns / 1e6)}`)),
      x.status_code === 2 ? h("p", { class: "error" }, `Error: ${x.status_message || "sin mensaje"}`) : null,
      h("dl", { class: "kv" }, Object.entries(x.attributes || {}).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, typeof v === "object" ? JSON.stringify(v) : String(v))])),
      (x.events || []).length ? h("h3", {}, "Eventos") : null,
      h("dl", { class: "kv" }, (x.events || []).flatMap((e) => [h("dt", {}, `+${fmtMs((e.ts_ns - t0) / 1e6)}`), h("dd", {}, `${e.name} ${attrLabel(e.attributes)}`)])),
    ].filter(Boolean)); // replaceChildren convertiría null en el texto "null"
  };
  const wf = h("div", { class: "waterfall card" }, ordered.map(({ span: x, depth }) => {
    const left = (100 * (x.start_ns - t0)) / total;
    const width = Math.max(0.3, (100 * x.duration_ns) / total);
    const row = h("div", { class: "wf-row", tabindex: 0 },
      h("div", { class: "wf-name", style: `padding-left: ${depth * 14}px`, title: `${x.service} · ${x.name}` }, x.name, h("small", {}, x.service)),
      h("div", { class: "wf-track" }, h("div", { class: `wf-bar ${x.status_code === 2 ? "err" : ""}`, style: `left: ${left}%; width: ${width}%; --c: ${color(x.service)}` })),
      h("div", { class: "wf-dur" }, fmtMs(x.duration_ns / 1e6)));
    row.addEventListener("click", () => showDetail(x, row));
    row.addEventListener("keydown", (e) => { if (e.key === "Enter") showDetail(x, row); });
    return row;
  }));
  root.append(wf, detail);

  const logs = h("section", {}, h("h2", {}, "Logs de la traza"));
  if (!r.logs.length) logs.append(h("p", { class: "muted" }, "No hay logs asociados a esta traza."));
  else logs.append(logTable(r.logs.slice().sort((a, b) => a.ts_ns - b.ts_ns), false));
  root.append(logs);
}

function logTable(rows, withTrace) {
  return h("div", { class: "table-wrap" }, h("table", {},
    h("thead", {}, h("tr", {}, ["Hora", "Nivel", "Servicio", "Mensaje", withTrace ? "Traza" : null].filter(Boolean).map((t) => h("th", {}, t)))),
    h("tbody", {}, rows.map((l) => h("tr", {},
      h("td", { class: "mono" }, fmtTime(l.ts_ns)),
      h("td", {}, h("span", { class: `chip ${severityClass(l.severity)}` }, l.severity_text || l.severity)),
      h("td", {}, l.service),
      h("td", { class: "body" }, l.body),
      withTrace ? h("td", {}, l.trace_id ? h("a", { href: `#traza/${l.trace_id}`, class: "mono" }, l.trace_id.slice(0, 8)) : "—") : null)))));
}

async function viewLogs(root) {
  const f = state.logFilter;
  const svcSel = h("select", { "aria-label": "Servicio", onchange: (e) => { f.service = e.target.value; render(); } }, await serviceOptions(f.service));
  const sevSel = h("select", { "aria-label": "Nivel", onchange: (e) => { f.severity = +e.target.value; render(); } },
    SEVERITY.map((x) => h("option", { value: x.min, selected: x.min === f.severity }, x.label)));
  const search = h("input", { type: "search", placeholder: "Buscar en el mensaje", value: f.q, "aria-label": "Buscar" });
  search.addEventListener("keydown", (e) => { if (e.key === "Enter") { f.q = search.value; render(); } });
  root.append(h("section", {}, h("div", { class: "row" }, h("h2", {}, "Logs"), svcSel, sevSel, search)));
  const r = await api("/api/logs", { since: state.win, service: f.service, severity: f.severity, q: f.q, limit: 500 });
  root.append(r.logs.length ? logTable(r.logs, true) : h("p", { class: "empty" }, "No hay logs que coincidan."));
}

async function viewMetricas(root) {
  const r = await api("/api/metrics", { since: state.win });
  if (!r.metrics.length) {
    root.append(h("p", { class: "empty" }, "No hay métricas en esta ventana."));
    return;
  }
  const names = [...new Map(r.metrics.map((m) => [m.name, m])).values()];
  if (!state.metric || !names.some((m) => m.name === state.metric)) state.metric = names[0].name;
  const list = h("div", { class: "metric-list card" }, names.map((m) =>
    h("button", { type: "button", class: m.name === state.metric ? "sel" : null, onclick: () => { state.metric = m.name; render(); } },
      m.name, h("small", {}, `${m.kind}${m.unit ? " · " + m.unit : ""} · ${r.metrics.filter((x) => x.name === m.name).map((x) => x.service).join(", ")}`))));
  const modes = ["auto", "value", "rate", "mean", "p50", "p95", "p99"];
  const modeSel = h("select", { "aria-label": "Modo", onchange: (e) => { state.metricMode = e.target.value; render(); } },
    modes.map((m) => h("option", { value: m, selected: m === state.metricMode }, m)));
  const chart = await metricCard(state.metric, state.metric, { mode: state.metricMode, width: chartWidth("wide") });
  root.append(h("div", { class: "split" }, list, h("div", { style: "display: grid; gap: 0.6rem; min-width: 0" }, h("div", { class: "row" }, h("span", { class: "muted" }, "Cálculo"), modeSel), chart)));
}

// -------------------------------------------------------------- navegación

function currentRoute() {
  const raw = location.hash.replace(/^#/, "");
  if (raw.startsWith("traza/")) return { tab: "trazas", trace: raw.slice(6) };
  return { tab: ["resumen", "trazas", "metricas", "logs"].includes(raw) ? raw : "resumen" };
}

let renderSeq = 0;
async function render() {
  const route = currentRoute();
  state.tab = route.tab;
  for (const a of document.querySelectorAll(".tabs a")) {
    if (a.dataset.tab === route.tab) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  const seq = ++renderSeq;
  const next = h("div", { style: "display: contents" });
  const line = document.getElementById("status-line");
  try {
    if (route.trace) await viewTraza(next, route.trace);
    else if (route.tab === "trazas") await viewTrazas(next);
    else if (route.tab === "metricas") await viewMetricas(next);
    else if (route.tab === "logs") await viewLogs(next);
    else await viewResumen(next);
    if (seq !== renderSeq) return; // llegó una vista más nueva
    document.getElementById("view").replaceChildren(next);
    line.textContent = `Actualizado a las ${new Date().toLocaleTimeString("es")} · ventana ${state.win}`;
  } catch (e) {
    if (e instanceof AuthError) return logout("El token fue rechazado. Vuelva a introducirlo.");
    line.textContent = `No se pudo actualizar: ${e.message}`;
  }
}

function schedule() {
  clearInterval(state.timer);
  // Una traza ya terminada no cambia: no se refresca.
  if (document.getElementById("auto").checked && !currentRoute().trace) state.timer = setInterval(render, 15000);
}

function logout(msg) {
  try { sessionStorage.removeItem(TOKEN_KEY); } catch (_) { /* almacenamiento no disponible */ }
  state.token = null;
  clearInterval(state.timer);
  document.getElementById("app").hidden = true;
  document.getElementById("login").hidden = false;
  const err = document.getElementById("login-error");
  err.hidden = !msg;
  err.textContent = msg || "";
  document.getElementById("token").focus();
}

function enter(token) {
  state.token = token;
  document.getElementById("login").hidden = true;
  document.getElementById("app").hidden = false;
  render();
  schedule();
}

document.addEventListener("DOMContentLoaded", () => {
  document.getElementById("login-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const t = document.getElementById("token").value.trim();
    state.token = t;
    try {
      await api("/api/status");
      try { sessionStorage.setItem(TOKEN_KEY, t); } catch (_) { /* sin almacenamiento: solo en memoria */ }
      enter(t);
    } catch (err) {
      const box = document.getElementById("login-error");
      box.hidden = false;
      box.textContent = err instanceof AuthError ? "Token incorrecto." : `No se pudo conectar: ${err.message}`;
    }
  });
  document.getElementById("window").addEventListener("change", (e) => { state.win = e.target.value; render(); });
  document.getElementById("refresh").addEventListener("click", render);
  document.getElementById("auto").addEventListener("change", schedule);
  document.getElementById("logout").addEventListener("click", () => logout(""));
  window.addEventListener("hashchange", () => { render(); schedule(); });
  let resizeTimer = null;
  let lastWidth = window.innerWidth;
  window.addEventListener("resize", () => {
    if (Math.abs(window.innerWidth - lastWidth) < 40 || !state.token) return;
    lastWidth = window.innerWidth;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(render, 250);
  });

  let saved = null;
  try { saved = sessionStorage.getItem(TOKEN_KEY); } catch (_) { /* nada */ }
  if (saved) enter(saved);
  else logout("");
});
