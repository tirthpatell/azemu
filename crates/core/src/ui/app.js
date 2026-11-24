// The inspector page: a read-only view of GET /_azemu/inspect.
// No framework and no network except that API. Every piece of data reaches the page through textContent
// or setAttribute of a plain value: names, message bodies and documents are whatever a test wrote and may contain markup.
"use strict";

const API = "/_azemu/inspect";
const REFRESH_MS = 2000;
const PAGE_SIZES = [10, 25, 50, 100, 250, 500];

const $ = (id) => document.getElementById(id);

// h("a", {href: "#/x", class: "k"}, "text", child) builds an element; strings become text nodes.
function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (v === null || v === undefined || v === false) continue;
      if (k === "on") {
        for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
      } else if (k === "text") {
        el.textContent = String(v);
      } else {
        el.setAttribute(k, v === true ? "" : String(v));
      }
    }
  }
  for (const kid of kids.flat()) {
    if (kid === null || kid === undefined || kid === false) continue;
    el.appendChild(typeof kid === "string" || typeof kid === "number" ? document.createTextNode(String(kid)) : kid);
  }
  return el;
}

// ---- state -----------------------------------------------------------------------------------------------------

const state = {
  path: "/",
  params: new URLSearchParams(),
  marker: null,
  filter: "",
  selectedKey: null,
  view: null, // a query of a "view" button (content=1) that is on
  limit: 50,
  loading: 0,
  seq: 0,
  columns: new Map(), // node path -> column names seen, so a table keeps its columns across refreshes
  markers: [], // markers of the pages before this one
  lastNode: null,
};

const treeNodes = new Map(); // path -> {li, children ul, loaded}

// ---- API -------------------------------------------------------------------------------------------------------

async function getJson(path, params) {
  const qs = params && params.toString() ? "?" + params.toString() : "";
  const url = API + (path === "/" ? "" : path) + qs;
  const resp = await fetch(url, { headers: { Accept: "application/json" }, cache: "no-store", credentials: "same-origin" });
  let body = null;
  try {
    body = await resp.json();
  } catch (_) {
    body = null;
  }
  if (!resp.ok) {
    const e = body && body.error ? body.error : {};
    const err = new Error((e.code ? e.code + ": " : "") + (e.message || "HTTP " + resp.status));
    err.status = resp.status;
    throw err;
  }
  return body;
}

function busy(delta) {
  state.loading += delta;
  $("loading").hidden = state.loading <= 0;
}

// ---- formatting ------------------------------------------------------------------------------------------------

function isObject(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function scalarText(v) {
  if (v === null || v === undefined) return "-";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

// What a table cell shows: the text of a body, the text of a value, or the compact JSON of anything else.
function cellText(v) {
  if (isObject(v) && typeof v.text === "string") return v.text.replace(/\s+/g, " ");
  if (isBody(v)) return "(binary, " + (v.size === undefined ? "?" : v.size) + " bytes)";
  return scalarText(v);
}

function short(text, n) {
  return text.length > n ? text.slice(0, n) + "..." : text;
}

function isBody(v) {
  return isObject(v) && (typeof v.text === "string" || typeof v.base64 === "string") && (v.size === undefined || typeof v.size === "number");
}

function bytesOf(body) {
  if ("base64" in body) {
    const bin = atob(body.base64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }
  return new TextEncoder().encode(body.text);
}

function hexDump(bytes) {
  const lines = [];
  for (let off = 0; off < bytes.length; off += 16) {
    const chunk = bytes.subarray(off, off + 16);
    const hex = Array.from(chunk, (b) => b.toString(16).padStart(2, "0")).join(" ");
    const asc = Array.from(chunk, (b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("");
    lines.push(off.toString(16).padStart(8, "0") + "  " + hex.padEnd(47, " ") + "  " + asc);
  }
  return lines.join("\n");
}

// A body (text or base64) as a <pre> with a switch between the text and a hex dump. Never HTML.
function bodyBox(body, label) {
  const pre = h("pre", { class: "body" });
  const info = h("div", { class: "muted" });
  let hex = false;
  const toggle = h("button", { type: "button" }, "show as hex");
  const paint = () => {
    if (hex) {
      pre.className = "hex";
      pre.textContent = hexDump(bytesOf(body));
    } else {
      pre.className = "";
      pre.textContent = "text" in body ? body.text : "(binary, " + body.shown + " bytes shown as base64)\n" + body.base64;
    }
    toggle.textContent = hex ? "show as text" : "show as hex";
  };
  toggle.addEventListener("click", () => {
    hex = !hex;
    paint();
  });
  paint();
  let line = typeof body.size === "number" ? body.size + " bytes" : "text";
  if (body.contentType) line += ", " + body.contentType;
  if (body.truncated) line += ", first " + body.shown + " shown";
  info.textContent = line;
  const kids = [h("div", { class: "toolbar" }, label ? h("strong", null, label) : null, info, toggle), pre];
  if (typeof body.decoded === "string") {
    kids.push(h("div", { class: "muted" }, "decoded from base64:"), h("pre", { text: body.decoded }));
  }
  return h("div", null, kids);
}

// ---- navigation ------------------------------------------------------------------------------------------------

function parseHash() {
  let hash = location.hash.replace(/^#/, "");
  if (!hash.startsWith("/")) hash = "/" + hash;
  const q = hash.indexOf("?");
  const path = (q < 0 ? hash : hash.slice(0, q)).replace(/\/+$/, "") || "/";
  const params = new URLSearchParams(q < 0 ? "" : hash.slice(q + 1));
  return { path, params };
}

function hashFor(path, params) {
  const qs = params && params.toString() ? "?" + params.toString() : "";
  return "#" + path + qs;
}

function applyHash() {
  const { path, params } = parseHash();
  const view = params.get("view");
  params.delete("view");
  const marker = params.get("marker");
  params.delete("marker");
  const limit = parseInt(params.get("limit") || "", 10);
  params.delete("limit");
  const same = path === state.path;
  state.path = path;
  state.params = params;
  state.marker = marker;
  state.limit = limit >= 1 && limit <= 500 ? limit : 50;
  state.view = view;
  if (!same) {
    state.filter = "";
    state.selectedKey = null;
    state.markers = [];
  }
  load(true);
}

function go(path, params) {
  const target = hashFor(path, params);
  if (location.hash === target) load(true);
  else location.hash = target;
}

// the query that asks the API for what is on screen
function apiParams() {
  const p = new URLSearchParams(state.params);
  if (state.marker) p.set("marker", state.marker);
  p.set("limit", String(state.limit));
  if (state.view) for (const [k, v] of new URLSearchParams(state.view)) p.set(k, v);
  return p;
}

// ---- loading and drawing the selected node -----------------------------------------------------------------------

async function load(fresh) {
  const my = ++state.seq;
  busy(1);
  const main = $("main");
  const scroll = main.scrollTop;
  try {
    let content;
    if (state.path === "/") content = await drawHome();
    else {
      const node = await getJson(state.path, apiParams());
      state.lastNode = node;
      content = drawNode(node);
      markCurrent(state.path);
    }
    if (my !== state.seq) return;
    main.replaceChildren(content);
    main.scrollTop = fresh ? 0 : scroll;
    if (state.path === "/") markCurrent("/");
    revealInTree(state.path);
  } catch (e) {
    if (my !== state.seq) return;
    main.replaceChildren(h("div", { class: "error", role: "alert" }, e.message || String(e)));
  } finally {
    busy(-1);
  }
  crumbs();
}

function crumbs() {
  const box = $("crumbs");
  const parts = state.path.split("/").filter(Boolean);
  const kids = [];
  let acc = "";
  for (const p of parts) {
    acc += "/" + p;
    kids.push(document.createTextNode(" / "), h("a", { href: "#" + acc }, safeDecode(p)));
  }
  box.replaceChildren(...kids);
}

function safeDecode(s) {
  try {
    return decodeURIComponent(s);
  } catch (_) {
    return s;
  }
}

function propTable(props) {
  const keys = Object.keys(props);
  if (!keys.length) return h("p", { class: "muted" }, "No properties.");
  return h(
    "table",
    { class: "props" },
    h(
      "tbody",
      null,
      keys.map((k) => {
        const v = props[k];
        let cell;
        if (isBody(v)) cell = bodyBox(v);
        else if (v !== null && typeof v === "object") cell = h("pre", { text: JSON.stringify(v, null, 2) });
        else cell = h("span", { class: "mono", text: scalarText(v) });
        return h("tr", null, h("th", { scope: "row" }, k), h("td", null, cell));
      }),
    ),
  );
}

function drawNode(node) {
  const root = h("div");
  root.appendChild(h("h1", null, node.title || node.path, h("span", { class: "kind" }, node.kind)));
  root.appendChild(h("div", { class: "muted mono" }, node.path));
  if (node.parameters && node.parameters.length) root.appendChild(parameterBar(node));
  if (node.views && node.views.length) {
    root.appendChild(
      h(
        "div",
        { class: "toolbar" },
        node.views.map((v) =>
          h(
            "button",
            {
              type: "button",
              on: {
                click: () => {
                  state.view = state.view === v.query ? null : v.query;
                  load(false);
                },
              },
            },
            state.view === v.query ? "hide: " + v.label : v.label,
          ),
        ),
      ),
    );
  }
  // what is inside comes first: on a queue the messages are what a reader came for, not its twenty settings
  if (node.children && node.children.length) {
    root.appendChild(h("h2", null, "Inside"));
    root.appendChild(
      h(
        "div",
        { class: "children" },
        node.children.map((c) =>
          h("a", { href: "#" + c.path }, c.name, c.count !== undefined && c.count !== null ? h("span", { class: "n" }, c.count) : null),
        ),
      ),
    );
  }
  const hasProps = Object.keys(node.properties || {}).length > 0;
  const hasRows = node.items && (node.items.length || node.next || state.marker);
  if (hasProps || !(hasRows || (node.children && node.children.length))) {
    root.appendChild(h("h2", null, "Properties"));
    root.appendChild(propTable(node.properties || {}));
  }
  if (node.content) {
    root.appendChild(h("h2", null, "Content"));
    root.appendChild(isBody(node.content) ? bodyBox(node.content) : h("pre", { text: JSON.stringify(node.content, null, 2) }));
  }
  if (node.items && (node.items.length || node.next || state.marker)) root.appendChild(drawItems(node));
  return root;
}

function parameterBar(node) {
  const inputs = node.parameters.map((name) =>
    h(
      "label",
      null,
      name,
      h("input", {
        type: "text",
        "data-param": name,
        value: state.params.get(name) || "",
        placeholder: name,
        on: { keydown: (ev) => ev.key === "Enter" && applyParams() },
      }),
    ),
  );
  return h("div", { class: "toolbar" }, inputs, h("button", { type: "button", on: { click: applyParams } }, "Apply"));
}

function applyParams() {
  const p = new URLSearchParams();
  for (const input of document.querySelectorAll("input[data-param]")) {
    if (input.value) p.set(input.getAttribute("data-param"), input.value);
  }
  state.markers = [];
  const q = new URLSearchParams(p);
  if (state.limit !== 50) q.set("limit", String(state.limit));
  go(state.path, q);
}

function columnsFor(node) {
  const known = state.columns.get(node.path) || [];
  const seen = new Set(known);
  for (const row of node.items) {
    if (!isObject(row)) continue;
    for (const k of Object.keys(row)) {
      if (k === "path" || seen.has(k)) continue;
      seen.add(k);
      known.push(k);
    }
  }
  // "name" first and the body right after it, whatever order the rows came in
  for (const first of ["body", "name"]) {
    const i = known.indexOf(first);
    if (i > 0) known.unshift(...known.splice(i, 1));
  }
  state.columns.set(node.path, known);
  // a column with nothing in it on this page is left out (the detail still shows every field)
  const used = known.filter((c) => c === "name" || node.items.some((r) => isObject(r) && r[c] !== null && r[c] !== undefined && r[c] !== ""));
  return used.slice(0, 12);
}

function rowKey(row, i) {
  if (!isObject(row)) return "#" + i;
  return row.path || (row.name !== undefined ? "n:" + row.name : null) || (row.id !== undefined ? "i:" + row.id : null) || "#" + i;
}

function drawItems(node) {
  const wrap = h("div");
  wrap.appendChild(h("h2", null, "Rows"));
  const cols = columnsFor(node);
  const filter = state.filter.toLowerCase();
  const rows = node.items.filter((r) => !filter || JSON.stringify(r).toLowerCase().includes(filter));
  const detail = h("div");

  const filterBox = h("input", {
    type: "text",
    value: state.filter,
    placeholder: "filter these rows",
    "aria-label": "Filter the rows on this page",
    on: {
      input: (ev) => {
        state.filter = ev.target.value;
        const pos = ev.target.selectionStart;
        const main = $("main");
        const scroll = main.scrollTop;
        main.replaceChildren(drawNode(node));
        main.scrollTop = scroll;
        const again = main.querySelector("input[aria-label^='Filter']");
        if (again) {
          again.focus();
          again.setSelectionRange(pos, pos);
        }
      },
    },
  });
  const sizeSelect = h(
    "select",
    {
      "aria-label": "Rows per page",
      on: {
        change: (ev) => {
          state.limit = parseInt(ev.target.value, 10);
          state.marker = null;
          state.markers = [];
          const q = new URLSearchParams(state.params);
          if (state.limit !== 50) q.set("limit", String(state.limit));
          go(state.path, q);
        },
      },
    },
    Array.from(new Set([...PAGE_SIZES, state.limit])).sort((a, b) => a - b).map((n) => h("option", { value: n, selected: n === state.limit }, n + " rows")),
  );
  const prev = h("button", { type: "button", disabled: state.markers.length === 0 && !state.marker, on: { click: pagePrev } }, "Previous");
  const next = h("button", { type: "button", disabled: !node.next, on: { click: () => pageNext(node.next) } }, "Next page");
  wrap.appendChild(
    h(
      "div",
      { class: "toolbar" },
      filterBox,
      sizeSelect,
      prev,
      next,
      h("span", { class: "muted" }, filter ? rows.length + " of " + node.items.length + " rows match" : node.items.length + " rows" + (node.next ? ", more on the next page" : "")),
    ),
  );

  if (!rows.length) {
    wrap.appendChild(h("p", { class: "muted" }, node.items.length ? "No row on this page matches the filter." : "No rows."));
    return wrap;
  }
  const table = h("table", { class: "rows" });
  // every column keeps a readable width; a table with many columns scrolls sideways instead of cutting its headings
  table.style.minWidth = cols.length * 140 + "px";
  table.appendChild(h("thead", null, h("tr", null, cols.map((c) => h("th", { scope: "col" }, c)))));
  const body = h("tbody");
  rows.forEach((row, i) => {
    const key = rowKey(row, i);
    const tr = h("tr", { tabindex: 0, "data-key": key, class: key === state.selectedKey ? "selected" : null });
    for (const c of cols) {
      const v = isObject(row) ? row[c] : c === "value" ? row : undefined;
      const text = cellText(v);
      const td = h("td", { title: text.length > 40 ? short(text, 400) : null });
      if (c === "name" && isObject(row) && row.path) td.appendChild(h("a", { href: "#" + row.path, text: short(text, 200) }));
      else td.textContent = short(text, 200);
      tr.appendChild(td);
    }
    const select = () => {
      state.selectedKey = key;
      for (const other of body.querySelectorAll("tr.selected")) other.classList.remove("selected");
      tr.classList.add("selected");
      detail.replaceChildren(drawDetail(row));
    };
    tr.addEventListener("click", (ev) => {
      if (ev.target.tagName !== "A") select();
    });
    tr.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        select();
      }
    });
    body.appendChild(tr);
    if (key === state.selectedKey) detail.replaceChildren(drawDetail(row));
  });
  table.appendChild(body);
  wrap.appendChild(h("div", { class: "scroll" }, table));
  wrap.appendChild(detail);
  return wrap;
}

function drawDetail(row) {
  const box = h("section", { class: "detail", "aria-label": "Row detail" });
  if (!isObject(row)) {
    box.appendChild(h("pre", { text: JSON.stringify(row, null, 2) }));
    return box;
  }
  box.appendChild(h("h3", null, row.name !== undefined ? scalarText(row.name) : "Row"));
  if (row.path) box.appendChild(h("div", null, h("a", { href: "#" + row.path }, "open " + row.path)));
  // plain values side by side in a grid, then what needs room (a body, nested values) below
  const grid = h("div", { class: "grid" });
  const rest = [];
  for (const [k, v] of Object.entries(row)) {
    if (k === "path") continue;
    if (v !== null && typeof v === "object") {
      const field = h("div", { class: "field" }, h("div", { class: "fname" }, k));
      field.appendChild(isBody(v) ? bodyBox(v) : h("pre", { text: JSON.stringify(v, null, 2) }));
      rest.push(field);
    } else {
      grid.appendChild(h("div", { class: "field" }, h("div", { class: "fname" }, k), h("div", { class: "mono", text: scalarText(v) })));
    }
  }
  box.appendChild(grid);
  for (const f of rest) box.appendChild(f);
  box.appendChild(h("div", { class: "field" }, h("div", { class: "fname" }, "as JSON"), h("pre", { text: JSON.stringify(row, null, 2) })));
  return box;
}

function pageNext(marker) {
  state.markers.push(state.marker || "");
  state.marker = marker;
  pushPage();
}

function pagePrev() {
  const before = state.markers.pop();
  state.marker = before || null;
  pushPage();
}

function pushPage() {
  const q = new URLSearchParams(state.params);
  if (state.marker) q.set("marker", state.marker);
  if (state.limit !== 50) q.set("limit", String(state.limit));
  if (state.view) q.set("view", state.view);
  const target = hashFor(state.path, q);
  history.replaceState(null, "", target);
  load(true);
}

// ---- the start page --------------------------------------------------------------------------------------------

function card(title, value, note) {
  return h("div", { class: "card" }, h("div", { class: "t" }, title), h("div", { class: "v" }, scalarText(value)), note ? h("div", { class: "muted" }, note) : null);
}

async function drawHome() {
  const [root, emu, config, unsupported, faults] = await Promise.all([
    getJson("/"),
    getJson("/_emulator"),
    getJson("/_emulator/config", new URLSearchParams({ limit: "500" })),
    getJson("/_emulator/unsupported", new URLSearchParams({ limit: "500" })),
    getJson("/_emulator/faults", new URLSearchParams({ limit: "500" })),
  ]);
  const out = h("div");
  out.appendChild(h("h1", null, "azemu " + scalarText(emu.properties.version), h("span", { class: "kind" }, "commit " + scalarText(emu.properties.commit))));
  out.appendChild(
    h(
      "div",
      { class: "cards" },
      card("Strict authorisation", emu.properties.strictAuth ? "on" : "off"),
      card("Fault rules", emu.properties.faultRules),
      card("Cross-site check", emu.properties.crossSiteCheck),
      card("Operations asked for and not built", unsupported.items.length),
    ),
  );

  out.appendChild(h("h2", null, "Services"));
  const svc = (root.children || []).filter((c) => c.kind === "service");
  out.appendChild(
    h(
      "table",
      { class: "props" },
      h(
        "tbody",
        null,
        svc.map((c) => h("tr", null, h("th", { scope: "row" }, h("a", { href: "#" + c.path }, c.name)), h("td", { class: "mono" }, c.count === undefined || c.count === null ? "" : c.count + " account(s)"))),
      ),
    ),
  );
  const without = (root.properties && root.properties.servicesWithoutInspector) || [];
  if (without.length) out.appendChild(h("p", { class: "muted" }, "No inspector (their state is shown by another service or has none): " + without.join(", ")));

  out.appendChild(h("h2", null, "Listeners"));
  const listeners = config.properties.listeners || [];
  out.appendChild(
    listeners.length
      ? h("table", { class: "rows" }, h("thead", null, h("tr", null, h("th", null, "kind"), h("th", null, "service"), h("th", null, "address"))), h("tbody", null, listeners.map((l) => h("tr", null, h("td", { text: l.kind }), h("td", { text: l.service || "all" }), h("td", { text: l.address })))))
      : h("p", { class: "muted" }, "None reported."),
  );

  out.appendChild(h("h2", null, "Settings that differ from the default"));
  const changed = config.items.filter((s) => s.source !== "default");
  out.appendChild(
    changed.length
      ? h("table", { class: "rows" }, h("thead", null, h("tr", null, h("th", null, "name"), h("th", null, "value"), h("th", null, "from"))), h("tbody", null, changed.map((s) => h("tr", null, h("td", { text: s.name }), h("td", { text: s.value }), h("td", { text: s.source })))))
      : h("p", { class: "muted" }, "None: every setting is at its default."),
  );
  const dir = config.properties.dataDirectory;
  out.appendChild(h("p", { class: "muted" }, dir ? "Data directory: " + dir : "No data directory: state lives in memory only."));

  out.appendChild(h("h2", null, "Operations asked for that are not built"));
  out.appendChild(
    unsupported.items.length
      ? h("table", { class: "rows" }, h("thead", null, h("tr", null, h("th", null, "service"), h("th", null, "operation"), h("th", null, "count"))), h("tbody", null, unsupported.items.map((u) => h("tr", null, h("td", { text: u.service }), h("td", { text: u.operation }), h("td", { text: u.count })))))
      : h("p", { class: "muted" }, "None so far."),
  );
  const stored = (unsupported.properties.storedOnly || []).filter((s) => s);
  if (stored.length) {
    out.appendChild(h("p", { class: "muted" }, "Resource types that were stored and not built: " + stored.map((s) => s.resourceType + " (" + s.count + ")").join(", ")));
  }

  out.appendChild(h("h2", null, "Fault rules"));
  out.appendChild(
    faults.items.length
      ? h("div", null, faults.items.map((r) => h("pre", { text: JSON.stringify(r, null, 2) })))
      : h("p", { class: "muted" }, "None. Rules are added with POST /_azemu/faults."),
  );
  return out;
}

// ---- the tree --------------------------------------------------------------------------------------------------

function treeItem(entry, depth) {
  const li = h("li", { role: "treeitem", "aria-expanded": "false" });
  const toggle = h("button", { type: "button", class: "toggle", "aria-label": "Expand " + entry.name }, "▸");
  const link = h("a", { class: "tlink", href: "#" + entry.path, title: entry.path }, entry.name);
  const row = h("div", { class: "row" }, toggle, link);
  if (entry.count !== undefined && entry.count !== null) row.appendChild(h("span", { class: "tcount" }, entry.count));
  const ul = h("ul", { role: "group", hidden: true });
  li.appendChild(row);
  li.appendChild(ul);
  const info = { li, ul, toggle, link, entry, loaded: false, depth, nextMarker: null };
  treeNodes.set(entry.path, info);
  toggle.addEventListener("click", () => toggleNode(info));
  link.addEventListener("click", () => {
    if (!info.loaded) expandNode(info);
  });
  return info;
}

async function expandNode(info) {
  info.ul.hidden = false;
  info.li.setAttribute("aria-expanded", "true");
  info.toggle.textContent = "▾";
  if (!info.loaded) {
    info.loaded = true;
    await fillChildren(info, null);
  }
}

function collapseNode(info) {
  info.ul.hidden = true;
  info.li.setAttribute("aria-expanded", "false");
  info.toggle.textContent = "▸";
}

function toggleNode(info) {
  if (info.ul.hidden) expandNode(info);
  else collapseNode(info);
}

async function fillChildren(info, marker) {
  const params = new URLSearchParams({ limit: "100" });
  if (marker) params.set("marker", marker);
  let node;
  try {
    node = await getJson(info.entry.path, params);
  } catch (e) {
    info.ul.appendChild(h("li", { class: "muted" }, "error: " + short(e.message, 80)));
    return;
  }
  if (info.more) {
    info.more.remove();
    info.more = null;
  }
  const entries = [];
  for (const c of node.children || []) entries.push({ name: c.name, path: c.path, count: c.count });
  // rows that have a path of their own are nodes too
  for (const r of node.items || []) {
    if (isObject(r) && typeof r.path === "string") entries.push({ name: scalarText(r.name !== undefined ? r.name : r.path), path: r.path, count: null });
  }
  for (const e of entries) {
    if (treeNodes.has(e.path) && treeNodes.get(e.path).li.isConnected) continue;
    const child = treeItem(e, info.depth + 1);
    info.ul.appendChild(child.li);
  }
  if (!entries.length && !marker) {
    info.toggle.classList.add("leaf");
  }
  if (node.next) {
    info.more = h("li", null, h("button", { type: "button", class: "more", on: { click: () => fillChildren(info, node.next) } }, "more..."));
    info.ul.appendChild(info.more);
  }
}

async function revealInTree(path) {
  // open every node on the way down to the selected one
  const parts = path.split("/").filter(Boolean);
  let info = treeNodes.get("/");
  if (!info) return;
  if (!info.loaded) await expandNode(info);
  let acc = "";
  for (const p of parts) {
    acc += "/" + p;
    // a node's path may skip a level; find the child that is a prefix of the target
    let next = treeNodes.get(acc);
    if (!next) {
      const candidates = Array.from(info.ul.children)
        .map((li) => Array.from(treeNodes.values()).find((n) => n.li === li))
        .filter((n) => n && (path === n.entry.path || path.startsWith(n.entry.path + "/")));
      next = candidates.sort((a, b) => b.entry.path.length - a.entry.path.length)[0];
    }
    if (!next || !next.li.isConnected) break;
    info = next;
    if (info.entry.path === path) break;
    if (!info.loaded) await expandNode(info);
  }
  markCurrent(path);
}

function markCurrent(path) {
  for (const n of treeNodes.values()) n.link.classList.toggle("current", n.entry.path === path);
  const cur = treeNodes.get(path);
  if (cur && cur.link.scrollIntoView) cur.link.scrollIntoView({ block: "nearest" });
}

async function buildTree() {
  const ul = $("treeroot");
  const rootInfo = treeItem({ name: "azemu", path: "/", count: null }, 0);
  ul.appendChild(rootInfo.li);
  rootInfo.loaded = true;
  rootInfo.ul.hidden = false;
  rootInfo.li.setAttribute("aria-expanded", "true");
  rootInfo.toggle.textContent = "▾";
  try {
    const root = await getJson("/");
    for (const c of root.children || []) {
      const child = treeItem({ name: c.name, path: c.path, count: c.count }, 1);
      rootInfo.ul.appendChild(child.li);
    }
  } catch (e) {
    rootInfo.ul.appendChild(h("li", { class: "muted" }, "error: " + e.message));
  }
}

// ---- start --------------------------------------------------------------------------------------------------------

let timer = null;

function setAuto(on) {
  if (timer) clearInterval(timer);
  timer = null;
  if (on) {
    timer = setInterval(() => {
      if (!document.hidden && state.loading === 0) load(false);
    }, REFRESH_MS);
  }
}

async function start() {
  $("refresh").addEventListener("click", () => load(false));
  $("auto").addEventListener("change", (ev) => setAuto(ev.target.checked));
  window.addEventListener("hashchange", applyHash);
  await buildTree();
  applyHash();
}

start();
