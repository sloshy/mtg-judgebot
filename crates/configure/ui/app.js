// judge-config's page. Every form here is generated from what the server
// sends: the judge.toml schema (from the loader's own types) and the .env
// registry (help from .env.example's comments). Nothing about the config's
// rules is restated; each draft goes to /api/check, which runs the loaders.

const token = new URLSearchParams(location.hash.slice(1)).get("token") || "";

let state = null; // GET /api/state
let tomlMode = "none"; // "none" | "form" | "text"
let form = null; // the judge.toml draft as JSON, in form mode
let formBase = ""; // JSON.stringify(form) as loaded, to know when it changed
let tomlText = ""; // the judge.toml draft, in text mode
let textEdited = false; // typed in since the Text view opened
let envDraft = {}; // setting name -> value, only the changed ones
let report = null; // the last /api/check answer
let tab = "models";
let flagged = { toml: new Map(), env: new Map(), text: null };
let savedNote = ""; // what to do after the last save, until the next edit

// ---------- plumbing ----------

async function call(path, body) {
  const res = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers: {
      "x-judge-config-token": token,
      ...(body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data = null;
  try {
    data = JSON.parse(text);
  } catch {
    data = { error: text };
  }
  if (!res.ok) {
    const err = new Error(data?.error || `${res.status} ${res.statusText}`);
    err.status = res.status;
    throw err;
  }
  return data;
}

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (k === "dataset") Object.assign(node.dataset, v);
    else if (v === true) node.setAttribute(k, "");
    else node.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    node.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return node;
}

/// Help text with `code` spans, as the doc comments write it.
function prose(text, cls = "help") {
  const p = el("p", { class: cls });
  const parts = String(text || "").split("`");
  return spans(p, parts);
}

/// A doc comment's prose: its hard wraps are not line breaks.
function doc(text, cls = "help") {
  return prose(String(text || "").replace(/([^\n])\n(?!\n)/g, "$1 "), cls);
}

function spans(p, parts) {
  parts.forEach((part, i) => {
    p.append(i % 2 ? el("code", { text: part }) : document.createTextNode(part));
  });
  return p;
}

function fatal(message) {
  const f = document.getElementById("fatal");
  f.textContent = message;
  f.hidden = !message;
}

// ---------- the schema ----------

function def(ref) {
  const name = ref.replace("#/$defs/", "");
  return state.schema.$defs[name];
}

/// A property's schema with `$ref` and `T | null` unwrapped.
function resolve(s) {
  if (!s) return {};
  if (s.$ref) return { ...def(s.$ref), description: s.description || def(s.$ref).description };
  if (s.anyOf) {
    const other = s.anyOf.find((x) => x.type !== "null");
    const inner = resolve(other);
    return { ...inner, description: s.description || inner.description, default: s.default };
  }
  return s;
}

function types(s) {
  return Array.isArray(s.type) ? s.type.filter((t) => t !== "null") : [s.type];
}

/// The values an enum takes, with their descriptions.
function choices(s) {
  if (s.oneOf?.every((x) => "const" in x)) {
    return s.oneOf.map((x) => ({ value: x.const, help: x.description }));
  }
  if (Array.isArray(s.enum)) return s.enum.map((v) => ({ value: v }));
  if ("const" in s) return [{ value: s.const, help: s.description }];
  return null;
}

function providerVariants() {
  return def("#/$defs/ProviderEntry").oneOf.map((v) => ({
    kind: v.properties.kind.const,
    schema: v,
  }));
}

function variantFor(kind) {
  return providerVariants().find((v) => v.kind === kind)?.schema;
}

// ---------- env lookups ----------

function envVar(name) {
  return state.env.vars.find((v) => v.name === name);
}

function envIsSet(name) {
  if (Object.hasOwn(envDraft, name)) return envDraft[name].trim() !== "";
  const v = envVar(name) || state.env.others.find((o) => o.name === name);
  return Boolean(v?.set);
}

// ---------- drafts ----------

function tomlDraft() {
  if (tomlMode === "form") return { mode: "form", value: form };
  if (tomlMode === "text") return { mode: "text", value: tomlText };
  return { mode: "none" };
}

function tomlChanged() {
  if (tomlMode === "form") return JSON.stringify(form) !== formBase;
  if (tomlMode === "text") return tomlText !== (state.toml.text ?? null);
  return false;
}

function pendingCount() {
  return Object.keys(envDraft).length + (tomlChanged() ? 1 : 0);
}

let checkSeq = 0;
let checkTimer = null;

function scheduleCheck() {
  // The review must show the draft a Save would send: until the next
  // report arrives, there is none.
  report = null;
  if (tab === "review") renderReview();
  const pending = pendingCount();
  document.getElementById("pending").textContent = pending || "";
  if (pending) savedNote = "";
  clearTimeout(checkTimer);
  checkTimer = setTimeout(runCheck, 250);
}

async function runCheck() {
  const seq = ++checkSeq;
  try {
    const r = await call("/api/check", { toml: tomlDraft(), env: envDraft });
    if (seq !== checkSeq) return;
    report = r;
    fatal("");
  } catch (e) {
    if (seq !== checkSeq) return;
    report = null;
    renderChecks(e.message);
    return;
  }
  renderChecks();
  applyFlags();
  if (tab === "review") renderReview();
}

// ---------- the checks panel ----------

const SURFACES = {
  database: "Database",
  models: "Models",
  bot: "Discord bot",
  api: "HTTP API",
};

function renderChecks(error) {
  const list = document.getElementById("check-list");
  const warnings = document.getElementById("warnings");
  list.replaceChildren();
  warnings.replaceChildren();
  flagged = { toml: new Map(), env: new Map(), text: null };
  if (error) {
    list.append(el("li", { class: "check error" }, el("div", { class: "msg", text: error })));
    return;
  }
  for (const c of report.checks) {
    const li = el(
      "li",
      { class: `check ${c.outcome}` },
      el("span", { class: "name", text: SURFACES[c.surface] || c.surface }),
      " ",
      el("span", {
        class: "state",
        text: { ok: "✓ starts", error: "✗ refuses", skipped: "– not checked" }[c.outcome],
      }),
    );
    const msg = c.outcome === "ok" ? c.summary : c.outcome === "error" ? c.message : c.reason;
    if (msg) li.append(el("div", { class: "msg", text: msg }));
    if (c.outcome === "error") {
      const loc = c.location;
      if (loc.in === "toml") flagged.toml.set(loc.path, c.message);
      if (loc.in === "env") flagged.env.set(loc.var, c.message);
      if (loc.in === "text") flagged.text = { span: loc.span, message: c.message };
      const where = describe(loc);
      if (where)
        li.append(
          el("div", { class: "msg" }, el("a", { onclick: () => reveal(loc), text: where })),
        );
    }
    list.append(li);
  }
  for (const w of report.warnings) warnings.append(el("li", { text: w }));
}

function describe(loc) {
  if (loc.in === "toml") return `judge.toml: ${loc.path}`;
  if (loc.in === "env") return `.env: ${loc.var}`;
  if (loc.in === "text") return "judge.toml text";
  return null;
}

/// Take the reader to the field an error is about.
function reveal(loc) {
  if (loc.in === "env") {
    const v = envVar(loc.var);
    if (v) {
      switchTab("settings");
      focusField(`[data-env="${CSS.escape(loc.var)}"]`);
    } else {
      switchTab("settings");
    }
    return;
  }
  switchTab("models");
  if (loc.in === "text" || tomlMode === "text") {
    if (tomlMode === "form") setTomlMode("text");
    const ta = document.querySelector("#tab-models textarea");
    if (ta && loc.span) {
      ta.focus();
      ta.setSelectionRange(loc.span[0], loc.span[1]);
    }
    return;
  }
  // The nearest field the path names: providers.x.auth, else providers.x.
  let path = loc.path;
  while (path) {
    if (document.querySelector(`[data-path="${CSS.escape(path)}"]`)) break;
    path = path.includes(".") ? path.slice(0, path.lastIndexOf(".")) : "";
  }
  if (path) focusField(`[data-path="${CSS.escape(path)}"]`);
}

function focusField(selector) {
  const node = document.querySelector(selector);
  if (!node) return;
  node.scrollIntoView({ block: "center", behavior: "smooth" });
  node.querySelector("input, select, textarea")?.focus({ preventScroll: true });
}

/// Mark the fields the current errors name.
function applyFlags() {
  for (const node of document.querySelectorAll(".field.flagged")) {
    node.classList.remove("flagged");
    node.querySelector(".err")?.remove();
  }
  const mark = (node, message) => {
    node.classList.add("flagged");
    node.append(el("div", { class: "err", text: message }));
  };
  for (const [path, message] of flagged.toml) {
    let p = path;
    while (p) {
      const node = document.querySelector(`.field[data-path="${CSS.escape(p)}"]`);
      if (node) {
        mark(node, message);
        break;
      }
      p = p.includes(".") ? p.slice(0, p.lastIndexOf(".")) : "";
    }
  }
  for (const [name, message] of flagged.env) {
    const node = document.querySelector(`.field[data-env="${CSS.escape(name)}"]`);
    if (node) mark(node, message);
  }
  const textErr = document.getElementById("text-error");
  if (textErr) {
    textErr.textContent = flagged.text ? flagged.text.message : "";
  }
}

// ---------- tabs ----------

function switchTab(name) {
  tab = name;
  for (const b of document.querySelectorAll(".tabs button")) {
    b.setAttribute("aria-selected", String(b.dataset.tab === name));
  }
  for (const t of ["models", "settings", "review"]) {
    document.getElementById(`tab-${t}`).hidden = t !== name;
  }
  if (name === "review") renderReview();
}

// ---------- generic fields ----------

/// One input for one schema property. `get`/`put` read and write the draft;
/// `put(undefined)` removes the key, which means "the loader's default".
function input(s, value, put, opts = {}) {
  const r = resolve(s);
  const opts2 = choices(r);
  const t = types(r);
  if (opts2) {
    const sel = el("select", {});
    if (!opts.required) {
      const d = r.default !== undefined ? `(default: ${r.default})` : "(default)";
      sel.append(el("option", { value: "", text: d }));
    }
    for (const o of opts2) {
      const unbuilt =
        opts.doors && !state.schema["x-built-doors"].includes(o.value) ? " (not built)" : "";
      sel.append(el("option", { value: o.value, text: `${o.value}${unbuilt}` }));
    }
    sel.value = value ?? "";
    sel.addEventListener("change", () => put(sel.value === "" ? undefined : sel.value));
    const wrap = el("div", {}, sel);
    const pick = opts2.find((o) => o.value === (value ?? r.default));
    if (pick?.help) wrap.append(doc(pick.help, "help small"));
    sel.addEventListener("change", () => {
      wrap.querySelector(".small")?.remove();
      const p = opts2.find((o) => o.value === (sel.value || r.default));
      if (p?.help) wrap.append(doc(p.help, "help small"));
    });
    return wrap;
  }
  if (t.includes("boolean")) {
    const sel = el("select", {});
    const d = r.default !== undefined ? `(default: ${r.default})` : "(default)";
    sel.append(el("option", { value: "", text: d }));
    sel.append(el("option", { value: "true", text: "true" }));
    sel.append(el("option", { value: "false", text: "false" }));
    sel.value = value === undefined ? "" : String(value);
    sel.addEventListener("change", () => put(sel.value === "" ? undefined : sel.value === "true"));
    return sel;
  }
  if (t.includes("integer") || t.includes("number")) {
    const box = el("input", {
      type: "number",
      step: t.includes("integer") ? "1" : "any",
      min: r.minimum,
      max: r.maximum,
      placeholder: opts.required ? "required" : "default",
    });
    box.value = value ?? "";
    box.addEventListener("input", () => {
      if (box.validity.badInput) return; // half-typed: wait for a number
      const raw = box.value.trim();
      const n = Number(raw);
      put(raw === "" || !Number.isFinite(n) ? undefined : n);
    });
    return box;
  }
  const box = el("input", {
    type: "text",
    spellcheck: "false",
    autocomplete: "off",
    placeholder: opts.required ? "required" : "default",
  });
  box.value = value ?? "";
  box.addEventListener("input", () => {
    put(box.value.trim() === "" ? undefined : box.value);
    if (r["x-env-var"] || s["x-env-var"]) badge();
  });
  if (!(r["x-env-var"] || s["x-env-var"])) return box;
  // The variable a key names: is it set in .env? (Never its value.)
  const status = el("span", { class: "badge" });
  const badge = () => {
    const name = box.value.trim();
    status.hidden = !name;
    const set = envIsSet(name);
    status.className = `badge ${set ? "ok" : "bad"}`;
    status.textContent = set ? "set in .env" : "not set in .env";
  };
  badge();
  return el("div", { class: "row" }, box, status);
}

function fieldRow(label, path, help, control, required) {
  return el(
    "div",
    { class: "field", dataset: { path } },
    el(
      "label",
      {},
      el("code", { text: label }),
      required ? el("span", { class: "req", text: "required" }) : null,
    ),
    control,
    help ? doc(help) : null,
  );
}

/// Every property of an object schema, as fields writing into `obj`.
function fields(schema, obj, path, opts = {}) {
  const out = [];
  const required = new Set(schema.required || []);
  for (const [key, prop] of Object.entries(schema.properties || {})) {
    if (opts.skip?.includes(key)) continue;
    const here = path ? `${path}.${key}` : key;
    let req = required.has(key);
    // A door-dependent key on an anthropic provider: shown only where the
    // door takes it, unless it is set (then shown, to be removed).
    const doors = prop["x-doors"];
    if (doors && opts.door) {
      const applies = doors[opts.door];
      if (applies === "no" && obj[key] === undefined) continue;
      req = applies === "required";
    }
    const r = resolve(prop);
    if (types(r).includes("object") && r.properties) {
      out.push(subTable(key, prop, r, obj, here));
      continue;
    }
    const control = input(
      prop,
      obj[key],
      (v) => {
        if (v === undefined) delete obj[key];
        else obj[key] = v;
        scheduleCheck();
        if (opts.onChange?.(key)) renderModels();
      },
      { required: req, doors: key === "endpoint" },
    );
    let help = prop.description || r.description;
    if (doors && opts.door && doors[opts.door] === "no") {
      help = `Does not apply to endpoint = "${opts.door}": remove it. ${help || ""}`;
    }
    out.push(fieldRow(key, here, help, control, req));
  }
  return out;
}

/// An optional table such as `[models.synth.pricing]`, behind a switch.
function subTable(key, prop, r, obj, path) {
  const on = obj[key] !== undefined;
  const toggle = el("input", { type: "checkbox", checked: on });
  toggle.addEventListener("change", () => {
    if (toggle.checked) obj[key] = {};
    else delete obj[key];
    scheduleCheck();
    renderModels();
  });
  const box = el(
    "div",
    { class: "field", dataset: { path } },
    el("label", {}, toggle, " ", el("code", { text: key })),
    doc(prop.description || r.description),
  );
  const wrap = el("div", {}, box);
  if (on) {
    wrap.append(el("div", { class: "card" }, fields(r, obj[key], path)));
  }
  return wrap;
}

// ---------- the models tab ----------

const MINIMAL = {
  providers: {
    anthropic: { kind: "anthropic", api_key_env: "ANTHROPIC_API_KEY" },
  },
  models: {
    extract: { provider: "anthropic", model: "claude-opus-5-5" },
    synth: { provider: "anthropic", model: "claude-opus-5-5" },
  },
};

async function setTomlMode(mode) {
  if (mode === "text" && tomlMode === "form") {
    // The form as the server renders it now, so the text starts from the
    // file with every form edit in it.
    let rendered;
    try {
      rendered = await call("/api/check", { toml: tomlDraft(), env: envDraft });
    } catch (e) {
      fatal(`Cannot show the text: ${e.message}`);
      return;
    }
    tomlText = rendered.toml.new ?? "";
    textEdited = false;
  }
  if (mode === "form" && tomlMode === "text") {
    if (textEdited && !confirm("Discard the text edits and return to the form?")) return;
  }
  tomlMode = mode;
  renderModels();
  scheduleCheck();
}

function renderModels() {
  const root = document.getElementById("tab-models");
  const scroll = window.scrollY;
  root.replaceChildren();
  if (tomlMode === "none") {
    root.append(
      el(
        "div",
        { class: "card" },
        el("h3", { text: "No judge.toml: the zero-config setup" }),
        prose(
          `There is no ${state.toml.path}. Every binary then runs Anthropic's API directly with ANTHROPIC_API_KEY, claude-opus-5-5 for both stages, and Voyage embeddings if VOYAGE_API_KEY is set. Its knobs are in Settings. A judge.toml chooses providers and models per stage.`,
          "desc",
        ),
        el(
          "div",
          { class: "row" },
          el("button", {
            class: "btn primary",
            type: "button",
            text: "Create judge.toml",
            onclick: () => {
              form = structuredClone(MINIMAL);
              formBase = "";
              tomlMode = "form";
              renderModels();
              scheduleCheck();
            },
          }),
          el("button", {
            class: "btn",
            type: "button",
            text: "Start from judge.example.toml as text",
            onclick: () => {
              tomlText = state.toml.example;
              textEdited = true;
              tomlMode = "text";
              renderModels();
              scheduleCheck();
            },
          }),
        ),
      ),
    );
    return;
  }
  const modeRow = el("div", { class: "row" });
  if (state.toml.form || form) {
    modeRow.append(
      el("button", {
        class: `btn${tomlMode === "form" ? " primary" : ""}`,
        type: "button",
        text: "Form",
        onclick: () => setTomlMode("form"),
        disabled: form === null,
      }),
      el("button", {
        class: `btn${tomlMode === "text" ? " primary" : ""}`,
        type: "button",
        text: "Text",
        onclick: () => setTomlMode("text"),
      }),
    );
  }
  modeRow.append(el("span", { class: "muted small", text: state.toml.path }));
  root.append(modeRow);
  if (state.toml.error) {
    root.append(
      prose(
        `The file on disk is not valid TOML, so it opens as text: ${state.toml.error}`,
        "notice",
      ),
    );
  }
  if (tomlMode === "text") {
    const ta = el("textarea", { class: "code", spellcheck: "false" });
    ta.value = tomlText;
    ta.addEventListener("input", () => {
      tomlText = ta.value;
      textEdited = true;
      scheduleCheck();
    });
    root.append(ta, el("p", { id: "text-error", class: "err" }));
    applyFlags();
    return;
  }
  root.append(renderProviders(), renderStages());
  applyFlags();
  window.scrollTo(0, scroll);
}

function renderProviders() {
  form.providers ??= {};
  const providers = form.providers;
  const section = el("div", {}, el("h2", { class: "section", text: "Providers" }));
  section.append(doc(state.schema.properties.providers.description, "muted"));
  for (const [name, entry] of Object.entries(providers)) {
    const variant = variantFor(entry.kind);
    const card = el("div", { class: "card", dataset: { path: `providers.${name}` } });
    const nameBox = el("input", { type: "text", spellcheck: "false" });
    nameBox.value = name;
    nameBox.addEventListener("change", () => renameProvider(name, nameBox.value.trim(), nameBox));
    card.append(
      el(
        "h3",
        {},
        el("span", { text: "[providers." }),
        nameBox,
        el("span", { text: "]" }),
        el("button", {
          class: "btn danger",
          type: "button",
          text: "Remove",
          onclick: () => {
            delete providers[name];
            scheduleCheck();
            renderModels();
          },
        }),
      ),
    );
    const kindSel = el("select", {});
    for (const v of providerVariants())
      kindSel.append(el("option", { value: v.kind, text: v.kind }));
    if (!variant)
      kindSel.append(el("option", { value: entry.kind ?? "", text: String(entry.kind) }));
    kindSel.value = entry.kind ?? "";
    kindSel.addEventListener("change", () => {
      // Another kind takes other keys: start it clean.
      providers[name] = { kind: kindSel.value };
      scheduleCheck();
      renderModels();
    });
    card.append(fieldRow("kind", `providers.${name}.kind`, variant?.description, kindSel, true));
    if (variant) {
      card.append(
        ...fields(variant, entry, `providers.${name}`, {
          skip: ["kind"],
          door: entry.kind === "anthropic" ? entry.endpoint || "direct" : null,
          onChange: (key) => key === "endpoint",
        }),
      );
    }
    section.append(card);
  }
  const newName = el("input", {
    type: "text",
    placeholder: "name, e.g. litellm",
    spellcheck: "false",
  });
  const newKind = el("select", {});
  for (const v of providerVariants()) newKind.append(el("option", { value: v.kind, text: v.kind }));
  section.append(
    el(
      "div",
      { class: "row" },
      newName,
      newKind,
      el("button", {
        class: "btn",
        type: "button",
        text: "Add provider",
        onclick: () => {
          const n = newName.value.trim();
          if (!n || Object.hasOwn(providers, n)) {
            newName.focus();
            return;
          }
          providers[n] = { kind: newKind.value };
          scheduleCheck();
          renderModels();
        },
      }),
    ),
  );
  return section;
}

function renameProvider(from, to, box) {
  const providers = form.providers;
  if (!to || to === from) {
    box.value = from;
    return;
  }
  if (Object.hasOwn(providers, to)) {
    box.value = from;
    alert(`There is already a provider named ${to}.`);
    return;
  }
  // Keep the order, and follow the stages that named it.
  form.providers = Object.fromEntries(
    Object.entries(providers).map(([k, v]) => (k === from ? [to, v] : [k, v])),
  );
  for (const stage of Object.values(form.models || {})) {
    if (stage && stage.provider === from) stage.provider = to;
  }
  scheduleCheck();
  renderModels();
}

function renderStages() {
  form.models ??= {};
  const models = form.models;
  const modelsSchema = def("#/$defs/ModelsEntry");
  const section = el("div", {}, el("h2", { class: "section", text: "Models" }));
  section.append(doc(state.schema.properties.models.description, "muted"));
  const required = new Set(modelsSchema.required || []);
  for (const [stage, prop] of Object.entries(modelsSchema.properties)) {
    const r = resolve(prop);
    const card = el("div", { class: "card", dataset: { path: `models.${stage}` } });
    const head = el("h3", {}, el("code", { text: `[models.${stage}]` }));
    card.append(head, doc(prop.description || r.description, "desc muted"));
    if (!required.has(stage)) {
      const toggle = el("input", { type: "checkbox", checked: models[stage] !== undefined });
      toggle.addEventListener("change", () => {
        if (toggle.checked) models[stage] = {};
        else delete models[stage];
        scheduleCheck();
        renderModels();
      });
      head.append(el("label", { class: "small muted" }, toggle, " use"));
    } else {
      models[stage] ??= {};
    }
    if (models[stage] !== undefined) {
      const entry = models[stage];
      for (const node of fields(r, entry, `models.${stage}`, { skip: ["provider"] }))
        card.append(node);
      card.insertBefore(providerPicker(stage, r, entry), card.children[2] || null);
    }
    section.append(card);
  }
  return section;
}

/// A stage's `provider`: the providers of a kind that can serve it.
function providerPicker(stage, r, entry) {
  const prop = r.properties.provider;
  const kinds = prop["x-provider-kinds"] || resolve(prop)["x-provider-kinds"] || [];
  const required = (r.required || []).includes("provider");
  const sel = el("select", {});
  if (!required) sel.append(el("option", { value: "", text: "(default: voyage)" }));
  const names = Object.entries(form.providers || {})
    .filter(([, p]) => kinds.includes(p.kind))
    .map(([n]) => n);
  for (const n of names) sel.append(el("option", { value: n, text: n }));
  if (entry.provider && !names.includes(entry.provider)) {
    sel.append(
      el("option", { value: entry.provider, text: `${entry.provider} (no such provider)` }),
    );
  }
  if (required && !entry.provider) sel.append(el("option", { value: "", text: "choose…" }));
  sel.value = entry.provider ?? "";
  sel.addEventListener("change", () => {
    if (sel.value === "") delete entry.provider;
    else entry.provider = sel.value;
    scheduleCheck();
  });
  const help = `${prop.description || ""} Kinds: ${kinds.join(", ")}.`;
  return fieldRow("provider", `models.${stage}.provider`, help, sel, required);
}

// ---------- the settings tab ----------

const GROUPS = {
  instance: "This instance",
  models: "Models without a judge.toml",
  pipeline: "Spend and pipeline",
  discord: "Discord bot",
  http: "HTTP API and web page",
  mcp: "MCP over HTTP",
  database: "Database",
  deployment: "Deployment",
};

function renderSettings() {
  const root = document.getElementById("tab-settings");
  root.replaceChildren();
  root.append(
    prose(
      `${state.env.path}${state.env.exists ? "" : " does not exist yet: saving creates it from .env.example"}. Secrets (keys, tokens, DATABASE_URL, the alert webhook) are never shown or written here; edit them in the file. Blank means the default.`,
      "notice",
    ),
  );
  if (state.env.error) {
    root.append(prose(`This .env cannot be edited as it stands: ${state.env.error}`, "notice"));
    return;
  }
  for (const [group, title] of Object.entries(GROUPS)) {
    const vars = state.env.vars.filter((v) => v.group === group);
    if (!vars.length) continue;
    const card = el("div", { class: "card" }, el("h3", { text: title }));
    let lastHelp = null;
    for (const v of vars) {
      const help = v.help && v.help !== lastHelp ? v.help : null;
      lastHelp = v.help;
      card.append(settingRow(v, help));
    }
    root.append(card);
  }
  if (state.env.others.length) {
    const card = el(
      "div",
      { class: "card" },
      el("h3", { text: "Other variables in .env" }),
      prose(
        "Provider keys and other variables this editor does not know. Never shown or written.",
        "desc muted",
      ),
    );
    for (const o of state.env.others) {
      card.append(
        el(
          "div",
          { class: "field", dataset: { env: o.name } },
          el("label", {}, el("code", { text: o.name })),
          el("span", { class: `badge ${o.set ? "ok" : ""}`, text: o.set ? "set" : "blank" }),
        ),
      );
    }
    root.append(card);
  }
  applyFlags();
}

function settingRow(v, help) {
  const label = el("label", {}, el("code", { text: v.name }));
  const helpNode = help ? el("details", {}, el("summary", { text: "about" }), prose(help)) : null;
  if (v.kind === "secret") {
    return el(
      "div",
      { class: "field", dataset: { env: v.name } },
      label,
      el(
        "div",
        { class: "row" },
        el("span", { class: `badge ${v.set ? "ok" : ""}`, text: v.set ? "set" : "not set" }),
        el("span", { class: "muted small", text: "secret: edit in .env" }),
      ),
      helpNode,
    );
  }
  if (v.hidden) {
    return el(
      "div",
      { class: "field", dataset: { env: v.name } },
      label,
      el("span", {
        class: "muted small",
        text: "set, not shown: it expands $ or carries credentials. Edit in .env.",
      }),
      helpNode,
    );
  }
  const current = Object.hasOwn(envDraft, v.name) ? envDraft[v.name] : v.value;
  const put = (value) => {
    if (value === v.value) delete envDraft[v.name];
    else envDraft[v.name] = value;
    scheduleCheck();
  };
  let control;
  const w = v.input;
  if (w.widget === "choice" || w.widget === "toggle") {
    const values = w.widget === "toggle" ? ["true", "false"] : w.values;
    control = el("select", {});
    control.append(el("option", { value: "", text: "(default)" }));
    for (const x of values) control.append(el("option", { value: x, text: x }));
    if (current && !values.includes(current))
      control.append(el("option", { value: current, text: current }));
    control.value = current;
    control.addEventListener("change", () => put(control.value));
  } else {
    control = el("input", {
      type: w.widget === "text" ? "text" : "number",
      step: w.widget === "integer" ? "1" : "any",
      min: w.widget === "integer" ? w.min : undefined,
      spellcheck: "false",
      autocomplete: "off",
      placeholder: "default",
    });
    control.value = current;
    control.addEventListener("input", () => {
      if (control.validity.badInput) return; // half-typed: wait for a number
      put(control.value.trim());
    });
  }
  return el("div", { class: "field", dataset: { env: v.name } }, label, control, helpNode);
}

// ---------- review ----------

/// Lines of `a` and `b` as a unified-looking diff (LCS), with unchanged
/// runs folded.
function diffLines(a, b) {
  const x = a === "" ? [] : a.split("\n");
  const y = b === "" ? [] : b.split("\n");
  const n = x.length;
  const m = y.length;
  const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = x[i] === y[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const ops = [];
  let i = 0;
  let j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && x[i] === y[j]) {
      ops.push([" ", x[i]]);
      i++;
      j++;
    } else if (j < m && (i >= n || lcs[i][j + 1] >= lcs[i + 1][j])) {
      ops.push(["+", y[j++]]);
    } else {
      ops.push(["-", x[i++]]);
    }
  }
  const pre = el("pre", { class: "diff" });
  const near = (k) => ops.slice(Math.max(0, k - 2), k + 3).some((o) => o[0] !== " ");
  let folded = false;
  ops.forEach(([op, line], k) => {
    if (op === " " && !near(k)) {
      if (!folded) pre.append(el("span", { class: "gap", text: "  …" }));
      folded = true;
      return;
    }
    folded = false;
    pre.append(
      el("span", { class: { "+": "add", "-": "del", " ": "" }[op], text: `${op} ${line}` }),
    );
  });
  return pre;
}

function renderReview() {
  const root = document.getElementById("tab-review");
  root.replaceChildren();
  if (!report) {
    root.append(prose("Checking…", "muted"));
    return;
  }
  const tomlOld = report.toml.old ?? "";
  const tomlNew = report.toml.new;
  const tomlDiffers = tomlNew !== null && tomlNew !== tomlOld;
  const envDiffers = report.env.changed;
  root.append(el("h2", { class: "section", text: state.toml.path }));
  root.append(tomlDiffers ? diffLines(tomlOld, tomlNew) : prose("No change.", "muted"));
  root.append(el("h2", { class: "section", text: state.env.path }));
  if (envDiffers) {
    const pre = el("pre", { class: "diff" });
    for (const d of report.env_diff) {
      pre.append(el("span", { class: d.op === "+" ? "add" : "del", text: `${d.op} ${d.line}` }));
    }
    if (!state.env.exists)
      pre.append(el("span", { class: "gap", text: "  (created from .env.example)" }));
    root.append(pre);
  } else {
    root.append(prose("No change.", "muted"));
  }
  const errors = report.checks.filter((c) => c.outcome === "error");
  const save = el("button", {
    class: "btn primary",
    type: "button",
    text: "Save",
    disabled: !(tomlDiffers || envDiffers),
    onclick: () => doSave(errors.length),
  });
  root.append(
    el(
      "div",
      { class: "row" },
      save,
      el("button", {
        class: "btn",
        type: "button",
        text: "Discard changes",
        disabled: pendingCount() === 0,
        onclick: () => {
          if (confirm("Discard every unsaved change?")) load();
        },
      }),
      errors.length
        ? el("span", {
            class: "small",
            text: `${errors.length} part(s) would refuse to start. Saving is allowed (you may not run them all).`,
          })
        : null,
    ),
  );
  if (savedNote) root.append(prose(savedNote, "notice"));
}

async function doSave(errors) {
  if (errors && !confirm("Some parts would refuse to start with these files. Save anyway?")) return;
  const tomlDiffers = tomlChanged();
  const envDiffers = Object.keys(envDraft).length > 0;
  try {
    await call("/api/save", {
      toml: tomlDraft(),
      env: envDraft,
      toml_hash: state.toml.hash,
      env_hash: state.env.hash,
    });
  } catch (e) {
    fatal(e.status === 409 ? `${e.message}` : `Not saved: ${e.message}`);
    return;
  }
  await load();
  switchTab("review");
  const steps = [];
  if (envDiffers)
    steps.push("`docker compose up -d` (recreates the containers whose environment changed)");
  if (tomlDiffers)
    steps.push("`docker compose restart bot api` (a judge.toml edit is not a change `up -d` sees)");
  savedNote = `Saved. Under Docker, apply it with ${steps.join(" and ")}. A cargo run binary reads the files when it starts.`;
  renderReview();
}

// ---------- start ----------

async function load() {
  if (!token) {
    fatal("No token: open the URL judge-config printed (it ends in #token=…).");
    return;
  }
  try {
    state = await call("/api/state");
  } catch (e) {
    fatal(e.message);
    return;
  }
  fatal("");
  envDraft = {};
  report = null;
  if (state.toml.exists && state.toml.form) {
    tomlMode = "form";
    form = state.toml.form;
    formBase = JSON.stringify(form);
  } else if (state.toml.exists) {
    tomlMode = "text";
    form = null;
    tomlText = state.toml.text;
    textEdited = false;
  } else {
    tomlMode = "none";
    form = null;
    formBase = "";
  }
  document.getElementById("paths").textContent = `Editing ${state.env.path} and ${state.toml.path}`;
  renderModels();
  renderSettings();
  scheduleCheck();
}

document.addEventListener("DOMContentLoaded", () => {
  for (const b of document.querySelectorAll(".tabs button")) {
    b.addEventListener("click", () => switchTab(b.dataset.tab));
  }
  window.addEventListener("beforeunload", (e) => {
    if (pendingCount() > 0) e.preventDefault();
  });
  load();
});
