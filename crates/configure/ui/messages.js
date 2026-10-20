// The page's text, one object per locale (D35). The server's replies (the
// checks, a refused draft) come in the language this page asks for; the help
// beside each field is the server's too (English doc comments, or a locale's
// override). Templates with values are functions, so each locale owns its word
// order and plurals; `code` spans are written in backquotes.
//
// Format, which a Rust test reads (crates/configure/src/catalog.rs): each locale
// is `const <ID> = {` on its own line, every message a `key:` at two spaces,
// closed by `};` alone on a line, and MESSAGES lists each as `  <id>: <ID>,`.
// The test holds every locale to English's keys, since nothing compiles this.

const EN = {
  noToken: "No token: open the URL judge-config printed (it ends in #token=…).",
  cannotShowText: (error) => `Cannot show the text: ${error}`,
  notSaved: (error) => `Not saved: ${error}`,
  paths: (env, toml) => `Editing ${env} and ${toml}`,

  tabModels: "Models · judge.toml",
  tabSettings: "Settings · .env",
  tabReview: "Review & save",
  checksTitle: "Would it start?",
  checkedBy: "Checked by the binaries' own loaders",
  leftToStartup: "Left to startup on the serving machine: cloud credentials, `--web`'s directory",

  surfaces: {
    database: "Database",
    models: "Models",
    roles: "Roles",
    discord: "Discord bot",
    http: "HTTP API",
    admin: "Admin interface",
    exposure: "Exposure",
  },
  outcome: {
    ok: "✓ starts",
    error: "✗ refuses",
    skipped: "– not checked",
  },
  whereToml: (path) => `judge.toml: ${path}`,
  whereEnv: (name) => `.env: ${name}`,
  whereText: "judge.toml text",

  defaultIs: (value) => `(default: ${value})`,
  defaultPlain: "(default)",
  notBuilt: (value) => `${value} (not built)`,
  placeholderRequired: "required",
  placeholderDefault: "default",
  required: "required",
  setInEnv: "set in .env",
  notSetInEnv: "not set in .env",
  doesNotApply: (endpoint, help) =>
    `Does not apply to endpoint = "${endpoint}": remove it. ${help || ""}`,

  zeroConfigTitle: "No judge.toml: the zero-config setup",
  zeroConfigLead: (path) => `No file at ${path}, so every binary runs:`,
  zeroConfigPoints: [
    "Anthropic's API directly, keyed by `ANTHROPIC_API_KEY`",
    "`claude-opus-5-5` for both stages",
    "Voyage embeddings, when `VOYAGE_API_KEY` is set",
  ],
  zeroConfigKnobs:
    "Its knobs (`ANTHROPIC_BASE_URL`, `VOYAGE_MODEL`, `VOYAGE_DIMENSIONS`) are in Settings. To choose providers and models per stage:",
  createToml: "Create judge.toml",
  startFromExample: "Start from judge.example.toml as text",
  modeForm: "Form",
  modeText: "Text",
  notValidToml: (error) => `The file on disk is not valid TOML, so it opens as text: ${error}`,
  confirmDiscardText: "Discard the text edits and return to the form?",
  providers: "Providers",
  remove: "Remove",
  providerNamePlaceholder: "name, e.g. litellm",
  addProvider: "Add provider",
  providerExists: (name) => `There is already a provider named ${name}.`,
  models: "Models",
  useStage: "use",
  defaultVoyage: "(default: voyage)",
  noSuchProvider: (name) => `${name} (no such provider)`,
  choose: "choose…",
  kinds: (help, kinds) => `${help} Kinds: ${kinds.join(", ")}.`,

  groups: {
    instance: "This instance",
    roles: "Roles",
    models: "Models without a judge.toml",
    pipeline: "Spend and pipeline",
    discord: "Discord bot",
    http: "HTTP API and web app",
    mcp: "MCP over HTTP",
    admin: "Admin interface",
    database: "Database",
    deployment: "Deployment",
  },
  settingsEditing: (path) => `Editing ${path}`,
  settingsCreates: (path) => `${path} does not exist yet: saving creates it from \`.env.example\``,
  settingsPoints: [
    "Secrets (API keys, tokens, `DATABASE_URL`, the alert webhook) are never shown",
    "Replace… writes a new value you type or paste; Cancel drops it before saving",
    "Blank means the default",
  ],
  envUneditable: (error) => `This .env cannot be edited as it stands: ${error}`,
  otherVariables: "Other variables in .env",
  otherPoints: [
    "Provider keys and other variables this editor does not know",
    "Never shown: replace one, or add a variable a `judge.toml` provider names",
  ],
  notInEnvYet: "not in .env yet",
  newVariablePlaceholder: "NAME, e.g. LITELLM_KEY",
  newVariableLabel: "New variable name",
  hasOwnField: (name) => `${name} has its own field above`,
  nameCharacters: "Letters, digits and _ only",
  addVariable: "Add variable",
  willReplace: "will be replaced",
  willSet: "will be set",
  isSet: "set",
  notSet: "not set",
  replaceButton: "Replace…",
  setButton: "Set…",
  newValuePlaceholder: "new value",
  valuePlaceholder: "value",
  newValueFor: (name) => `New value for ${name}`,
  show: "Show",
  hide: "Hide",
  cancel: "Cancel",
  about: "about",
  hiddenWhy: "not shown: it expands $ or carries credentials",

  checking: "Checking…",
  noChange: "No change.",
  createdFromExample: "  (created from .env.example)",
  save: "Save",
  discard: "Discard changes",
  confirmDiscardAll: "Discard every unsaved change?",
  refusals: (n) =>
    `${n} ${n === 1 ? "part" : "parts"} would refuse to start. Saving is still allowed.`,
  confirmSaveErrors: "Some parts would refuse to start with these files. Save anyway?",
  savedApply: "Saved. To apply it:",
  savedEnv:
    "Docker, `.env` changed: `docker compose up -d` (recreates the containers whose environment changed)",
  savedToml:
    "Docker, `judge.toml` changed: `docker compose restart judgebot` (`up -d` does not see a mounted file's edit)",
  savedCargo: "`cargo run`: restart the binaries; they read both files at startup",
};

/** Every locale the page has text for, by id. */
export const MESSAGES = {
  en: EN,
};

/** The first of `languages` (most preferred first) the page has text for,
 * by its language subtag; else `fallback` when it has text for it; else
 * English. */
export function pickLocale(languages, fallback) {
  for (const tag of languages || []) {
    const id = String(tag).split(/[-_]/)[0].toLowerCase();
    if (Object.hasOwn(MESSAGES, id)) return id;
  }
  return Object.hasOwn(MESSAGES, fallback) ? fallback : "en";
}
