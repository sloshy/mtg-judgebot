//! The editor's HTTP side: the page, and three calls.
//!
//! - `GET /api/state`: the schema, the files as they are, every variable's
//!   help and (for a setting) its value, or (for a secret) only whether it
//!   is set.
//! - `POST /api/check`: a draft rendered as the files it would write, and
//!   every surface's verdict on them (`check::run`). Nothing is written.
//! - `POST /api/save`: the same, written, unless either file changed since
//!   the page read it.
//!
//! A draft carries shown settings (`env`) and write-only replacements
//! (`replace`). A replacement's value goes into the file and nowhere else:
//! every reply is scrubbed of it, as of every secret already in the file,
//! and the `.env` diff names it without a value.
//!
//! The calls need the token printed at startup, in a header a page from
//! another origin cannot set without a CORS preflight this server never
//! answers; the `Host` must be a loopback name (or one passed with
//! `--allow-host`), so a rebound DNS name gets nothing either. The files are
//! read fresh on every call and written whole through a rename.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::Json as Body;
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    check,
    env::{self, DotEnv, Kind, Setting, Shown, VARS},
    toml_doc,
};

/// The header carrying the token.
pub const TOKEN_HEADER: &str = "x-judge-config-token";

/// What the server edits and who may ask.
#[derive(Debug)]
pub struct Editor {
    /// The `.env` file.
    pub env_path: PathBuf,
    /// The `judge.toml` file.
    pub toml_path: PathBuf,
    /// The token the page must send.
    pub token: String,
    /// `Host` names accepted besides the loopback ones.
    pub allowed_hosts: Vec<String>,
    /// The value `JUDGE_CONFIG` must hold for the containers to read
    /// `toml_path`, as a path relative to the `.env`'s directory when it can
    /// be one.
    pub toml_as_judge_config: String,
    /// Serializes saves.
    pub lock: Mutex<()>,
}

/// The router.
pub fn router(editor: Arc<Editor>) -> Router {
    let api = Router::new()
        .route("/api/state", get(state))
        .route("/api/check", post(check_draft))
        .route("/api/save", post(save))
        .route_layer(middleware::from_fn_with_state(editor.clone(), guard));
    Router::new()
        .route(
            "/",
            get(|| async { page("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/app.js",
            get(|| async { page("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/app.css",
            get(|| async { page("text/css; charset=utf-8", APP_CSS) }),
        )
        .merge(api)
        .layer(middleware::from_fn_with_state(editor.clone(), host_check))
        .with_state(editor)
}

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");

fn page(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
            ),
        ],
        body,
    )
        .into_response()
}

fn is_allowed_host(editor: &Editor, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    // Strip the port; an IPv6 literal keeps its brackets.
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.contains(']') => name,
        _ => host,
    };
    matches!(name, "localhost" | "127.0.0.1" | "[::1]")
        || editor
            .allowed_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
}

async fn host_check(State(editor): State<Arc<Editor>>, req: Request, next: Next) -> Response {
    if is_allowed_host(&editor, req.headers()) {
        next.run(req).await
    } else {
        (
            StatusCode::MISDIRECTED_REQUEST,
            "judge-config answers on localhost only (pass --allow-host NAME to add one)",
        )
            .into_response()
    }
}

async fn guard(State(editor): State<Arc<Editor>>, req: Request, next: Next) -> Response {
    let sent = req
        .headers()
        .get(TOKEN_HEADER)
        .map(axum::http::HeaderValue::as_bytes)
        .unwrap_or_default();
    let want = editor.token.as_bytes();
    // Constant time over the expected length.
    let same = sent.len() == want.len()
        && sent.iter().zip(want).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if same {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            "missing or wrong token: open the URL judge-config printed",
        )
            .into_response()
    }
}

/// An error the page shows as is.
#[derive(Debug, thiserror::Error)]
#[error("{1}")]
struct Failure(StatusCode, String);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Body(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(message: &dyn std::fmt::Display) -> Failure {
    Failure(StatusCode::UNPROCESSABLE_ENTITY, message.to_string())
}

fn io(path: &Path, e: &std::io::Error) -> Failure {
    Failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{}: {e}", path.display()),
    )
}

/// A file's text, `None` when it does not exist.
fn read(path: &Path) -> Result<Option<String>, Failure> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, &e)),
    }
}

fn digest(text: Option<&str>) -> String {
    text.map_or_else(String::new, |t| {
        Sha256::digest(t.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            })
    })
}

async fn state(State(editor): State<Arc<Editor>>) -> Result<Body<Json>, Failure> {
    let toml_text = read(&editor.toml_path)?;
    let env_text = read(&editor.env_path)?;
    let help = env::help(env::EXAMPLE);
    let (dotenv, env_error) = match env_text.as_deref().map(DotEnv::parse) {
        Some(Ok(d)) => (Some(d), None),
        Some(Err(e)) => (None, Some(e.to_string())),
        None => (DotEnv::parse(env::EXAMPLE).ok(), None),
    };
    let is_set = |name: &str| dotenv.as_ref().is_some_and(|d| d.is_set(name));
    let vars: Vec<Json> = VARS
        .iter()
        .map(|v| {
            let set = is_set(v.name);
            let mut entry = serde_json::Map::new();
            entry.insert("name".to_owned(), json!(v.name));
            entry.insert("group".to_owned(), json!(v.group));
            entry.insert("help".to_owned(), json!(help.get(v.name)));
            entry.insert("set".to_owned(), json!(set));
            if let (Kind::Setting(widget), Some(setting)) = (v.kind, Setting::named(v.name)) {
                entry.insert("kind".to_owned(), json!("setting"));
                entry.insert("input".to_owned(), json!(widget));
                let shown = dotenv.as_ref().map_or(Shown::Unset, |d| d.setting(setting));
                let (value, hidden) = match shown {
                    Shown::Unset => ("", false),
                    Shown::Value(v) => (v, false),
                    Shown::Hidden => ("", true),
                };
                entry.insert("value".to_owned(), json!(value));
                entry.insert("hidden".to_owned(), json!(hidden));
            } else {
                entry.insert("kind".to_owned(), json!("secret"));
            }
            Json::Object(entry)
        })
        .collect();
    let others: Vec<Json> = dotenv
        .iter()
        .flat_map(DotEnv::names)
        .filter(|n| env::var(n).is_none())
        .map(|n| json!({ "name": n, "set": is_set(n) }))
        .collect();
    let (toml_json, toml_error) = match toml_text.as_deref().map(toml_doc::parse) {
        Some(Ok(doc)) => (Some(toml_doc::to_json(&doc)), None),
        Some(Err(e)) => (None, Some(e.to_string())),
        None => (None, None),
    };
    let secrets: Vec<String> = dotenv
        .iter()
        .flat_map(DotEnv::sensitive)
        .map(str::to_owned)
        .collect();
    let mut out = json!({
        "toml": {
            "path": editor.toml_path.display().to_string(),
            "exists": toml_text.is_some(),
            "text": toml_text,
            "form": toml_json,
            "error": toml_error,
            "hash": digest(toml_text.as_deref()),
            "as_judge_config": editor.toml_as_judge_config,
            "example": include_str!("../../../judge.example.toml"),
        },
        "env": {
            "path": editor.env_path.display().to_string(),
            "exists": env_text.is_some(),
            "error": env_error,
            "hash": digest(env_text.as_deref()),
            "vars": vars,
            "others": others,
        },
    });
    redact(&mut out, &secrets);
    if let Some(root) = out.as_object_mut() {
        root.insert("schema".to_owned(), judge_bot::config::file_schema());
    }
    Ok(Body(out))
}

/// The ways a message can spell `secret`: as is, and as Rust's `{:?}` and
/// `escape_default` write it (`"` and `\` escaped), which is how the loaders
/// quote a value they reject (`JUDGE_SOURCE_URL="…": not an http(s) URL`).
fn spellings(secret: &str) -> Vec<String> {
    let mut forms = vec![
        secret.to_owned(),
        secret.escape_debug().to_string(),
        secret.escape_default().to_string(),
    ];
    forms.dedup();
    forms
}

/// Mask every occurrence of a value the page must not see
/// ([`DotEnv::sensitive`]) in every string of `v`. The loaders' messages
/// quote values they reject (`GUILD_ID must be …, got "…"`), and a setting
/// may expand to a secret, so the whole reply goes through this.
fn redact(v: &mut Json, secrets: &[String]) {
    match v {
        Json::String(s) => {
            for secret in secrets {
                for form in spellings(secret) {
                    if s.contains(form.as_str()) {
                        *s = s.replace(form.as_str(), "<redacted>");
                    }
                }
            }
        }
        Json::Array(a) => a.iter_mut().for_each(|x| redact(x, secrets)),
        Json::Object(o) => o.values_mut().for_each(|x| redact(x, secrets)),
        Json::Null | Json::Bool(_) | Json::Number(_) => {}
    }
}

/// The page's draft.
#[derive(Debug, Deserialize)]
struct Draft {
    /// The `judge.toml`: the form's JSON, raw text, or absent.
    toml: TomlDraft,
    /// Shown settings to change, by name.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Variables to overwrite write-only, by name: secrets, hidden settings,
    /// variables the registry does not know. Never sent back.
    #[serde(default)]
    replace: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum TomlDraft {
    /// No file: the zero-config setup. Only while there is none.
    None,
    /// The form's JSON, merged into the file.
    Form { value: Json },
    /// The text as typed.
    Text { value: String },
}

/// The files a draft would write.
struct Rendered {
    toml_old: Option<String>,
    toml_new: Option<String>,
    env_old: Option<String>,
    env_new: String,
    env: DotEnv,
    /// What the reply must not show: the sensitive values before and after,
    /// and every replacement.
    secrets: Vec<String>,
    /// The variables overwritten write-only.
    replaced: Vec<String>,
}

fn render(editor: &Editor, draft: &Draft) -> Result<Rendered, Failure> {
    let toml_old = read(&editor.toml_path)?;
    let toml_new = match (&draft.toml, &toml_old) {
        (TomlDraft::None, None) => None,
        (TomlDraft::None, Some(_)) => {
            return Err(bad(&format!(
                "{} exists; this editor never deletes it (remove it yourself for the zero-config setup)",
                editor.toml_path.display()
            )));
        }
        (TomlDraft::Text { value }, _) => Some(value.clone()),
        (TomlDraft::Form { value }, Some(old)) => {
            let mut doc = toml_doc::parse(old).map_err(|e| {
                bad(&format!(
                    "the file on disk is not TOML ({e}); edit it as text"
                ))
            })?;
            toml_doc::apply(&mut doc, value).map_err(|e| bad(&e))?;
            Some(doc.to_string())
        }
        (TomlDraft::Form { value }, None) => {
            Some(toml_doc::render_new(value).map_err(|e| bad(&e))?)
        }
    };
    let env_old = read(&editor.env_path)?;
    let mut env = DotEnv::parse(env_old.as_deref().unwrap_or(env::EXAMPLE)).map_err(|e| bad(&e))?;
    let mut secrets: Vec<String> = env.sensitive().into_iter().map(str::to_owned).collect();
    for (name, value) in &draft.env {
        let setting = Setting::named(name)
            .ok_or_else(|| bad(&format!("{name} is not a setting this editor writes")))?;
        let value = env::Value::new(setting, value).map_err(|e| bad(&e))?;
        env.set(setting, &value).map_err(|e| bad(&e))?;
    }
    let mut replaced = vec![];
    for (name, raw) in &draft.replace {
        let var = env::Name::new(name).ok_or_else(|| {
            bad(&format!(
                "{name:?} is not a variable name (letters, digits, _)"
            ))
        })?;
        // A shown setting is changed from the value the page displayed.
        if let Some(s) = Setting::named(name)
            && env.setting(s) != Shown::Hidden
        {
            return Err(bad(&format!("{name} is shown: change it in its field")));
        }
        let value = env::Value::replacement(&var, raw).map_err(|e| bad(&e))?;
        env.replace(&var, &value).map_err(|e| bad(&e))?;
        // As `DotEnv::sensitive`: a value under four characters is not worth
        // garbling every message over.
        if value.as_str().len() >= 4 {
            secrets.push(value.as_str().to_owned());
        }
        replaced.push(name.clone());
    }
    let env_new = env.render();
    // Read back as the binaries will: a `${NAME}` elsewhere sees the edit.
    let env = DotEnv::parse(&env_new).map_err(|e| bad(&e))?;
    secrets.extend(env.sensitive().into_iter().map(str::to_owned));
    // Longest first, so a secret containing another is masked whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    Ok(Rendered {
        toml_old,
        toml_new,
        env_old,
        env_new,
        env,
        secrets,
        replaced,
    })
}

/// Warnings about the files together, which no single loader sees.
fn warnings(editor: &Editor, r: &Rendered) -> Vec<String> {
    let mut out = vec![];
    let lookup = r.env.lookup();
    let judge_config = lookup(judge_bot::config::CONFIG_ENV).unwrap_or_default();
    let judge_config = judge_config.trim();
    let same_path = |a: &str, b: &str| a.trim_start_matches("./") == b.trim_start_matches("./");
    if r.toml_new.is_some() && judge_config.is_empty() {
        out.push(format!(
            "JUDGE_CONFIG is blank, so the containers will not read {} (cargo run will). Set JUDGE_CONFIG={} for Docker.",
            editor.toml_path.display(),
            editor.toml_as_judge_config
        ));
    } else if !judge_config.is_empty() && !same_path(judge_config, &editor.toml_as_judge_config) {
        out.push(format!(
            "JUDGE_CONFIG={judge_config} names another file than the one edited here ({}).",
            editor.toml_as_judge_config
        ));
    }
    // `cargo run` on this machine reaches Postgres through DATABASE_URL,
    // which compose's DB_PORT does not change: say when they disagree. Only
    // the ports are compared and named, never the URL.
    let port = |v: Option<String>| v.map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
    let published = port(lookup("DB_PORT")).unwrap_or_else(|| "5432".to_owned());
    if let Some(url) = lookup("DATABASE_URL").and_then(|u| url::Url::parse(u.trim()).ok())
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        && url.port().unwrap_or(5432).to_string() != published
    {
        out.push(format!(
            "DB_PORT publishes Postgres on {published}, but DATABASE_URL points at port {}: change the port in DATABASE_URL too (this editor does not write it).",
            url.port().unwrap_or(5432)
        ));
    }
    // The deprecated API_INTERFACES, in the words judgebot logs it with.
    let (roles_env, api_env) = (
        lookup(judgebot::roles::ROLES_ENV),
        lookup(judgebot::roles::API_INTERFACES_ENV),
    );
    if let Ok((roles, _)) = judgebot::roles::compose_roles(roles_env.as_deref(), api_env.as_deref())
        && let Some(w) = judgebot::roles::api_interfaces_warning(
            api_env.as_deref(),
            roles_env.as_deref(),
            &roles,
        )
    {
        out.push(w);
    }
    out
}

fn report(editor: &Editor, r: &Rendered) -> Json {
    let checks = check::run(r.toml_new.as_deref(), &r.env.lookup());
    let mut out = json!({
        "toml": { "old": r.toml_old, "new": r.toml_new },
        "env": { "changed": r.env_old.as_deref() != Some(r.env_new.as_str()) },
        "env_diff": env_diff(r.env_old.as_deref(), &r.env_new, &r.replaced),
        "checks": checks,
        "warnings": warnings(editor, r),
    });
    redact(&mut out, &r.secrets);
    out
}

/// The `.env` lines that change. A shown setting's line is shown; a
/// replaced variable is named with `~` and no value; any other line is
/// never shown, whatever it holds.
fn env_diff(old: Option<&str>, new: &str, replaced: &[String]) -> Vec<Json> {
    let old_lines: Vec<&str> = old.map(|o| o.lines().collect()).unwrap_or_default();
    let shown = |line: &str| {
        env::assignment(line).is_some_and(|(name, _)| {
            Setting::named(name).is_some() && !replaced.iter().any(|r| r == name)
        })
    };
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = vec![];
    for l in &old_lines {
        if !new_lines.contains(l) && shown(l) {
            out.push(json!({ "op": "-", "line": l }));
        }
    }
    for l in &new_lines {
        if !old_lines.contains(l) && shown(l) {
            out.push(json!({ "op": "+", "line": l }));
        }
    }
    for name in replaced {
        out.push(json!({ "op": "~", "line": format!("{name}: new value (not shown)") }));
    }
    out
}

async fn check_draft(
    State(editor): State<Arc<Editor>>,
    Body(draft): Body<Draft>,
) -> Result<Body<Json>, Failure> {
    let r = render(&editor, &draft)?;
    Ok(Body(report(&editor, &r)))
}

#[derive(Debug, Deserialize)]
struct Save {
    #[serde(flatten)]
    draft: Draft,
    toml_hash: String,
    env_hash: String,
}

async fn save(
    State(editor): State<Arc<Editor>>,
    Body(save): Body<Save>,
) -> Result<Body<Json>, Failure> {
    let _held = editor.lock.lock().await;
    let r = render(&editor, &save.draft)?;
    if digest(r.toml_old.as_deref()) != save.toml_hash
        || digest(r.env_old.as_deref()) != save.env_hash
    {
        return Err(Failure(
            StatusCode::CONFLICT,
            "a file changed on disk since this page read it; reload to see it".to_owned(),
        ));
    }
    // Both files are staged before either is replaced, so a failure to
    // write one leaves both as they were.
    let mut staged = vec![];
    let outcome = (|| {
        if let Some(new) = &r.toml_new
            && r.toml_old.as_deref() != Some(new.as_str())
        {
            staged.push(stage(&editor.toml_path, new, false)?);
        }
        if r.env_old.as_deref() != Some(r.env_new.as_str()) {
            staged.push(stage(&editor.env_path, &r.env_new, true)?);
        }
        Ok::<_, Failure>(())
    })();
    if let Err(e) = outcome {
        for s in &staged {
            let _ = fs::remove_file(&s.tmp);
        }
        return Err(e);
    }
    let mut done = vec![];
    for s in staged {
        if let Err(e) = fs::rename(&s.tmp, &s.target) {
            let _ = fs::remove_file(&s.tmp);
            let saved = if done.is_empty() {
                String::new()
            } else {
                format!(" ({} was saved)", done.join(", "))
            };
            return Err(Failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{}: {e}{saved}", s.target.display()),
            ));
        }
        done.push(s.target.display().to_string());
    }
    Ok(Body(report(&editor, &r)))
}

/// A file written beside its target, waiting to be renamed over it.
struct Staged {
    tmp: PathBuf,
    target: PathBuf,
}

/// Write `text` beside `path` for a rename over it. A symlink is followed,
/// so the file it names is what changes. The temporary file is new (never a
/// leftover or a planted link), uniquely named, and owner-only from its
/// creation; it then takes the old file's permissions, or for a new file
/// owner-only (`.env`, which holds secrets) or the usual mode (`judge.toml`).
fn stage(path: &Path, text: &str, secret: bool) -> Result<Staged, Failure> {
    use std::io::Write as _;
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.judge-config.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let perms = fs::metadata(&target).map(|m| m.permissions()).ok();
    let mut file = private(&tmp).map_err(|e| io(&tmp, &e))?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| match perms {
            Some(p) => fs::set_permissions(&tmp, p),
            None if !secret => readable().map_or(Ok(()), |p| fs::set_permissions(&tmp, p)),
            None => Ok(()),
        });
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(io(&tmp, &e));
    }
    Ok(Staged { tmp, target })
}

#[cfg(unix)]
fn private(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the non-unix stub, which has no mode to give"
)]
fn readable() -> Option<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn readable() -> Option<fs::Permissions> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    /// A scratch directory holding `.env`, removed on drop.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn editor(env: &str) -> Result<(Scratch, Editor), Box<dyn std::error::Error>> {
        let dir =
            std::env::temp_dir().join(format!("judge-config-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(".env"), env)?;
        let editor = Editor {
            env_path: dir.join(".env"),
            toml_path: dir.join("judge.toml"),
            token: "t".to_owned(),
            allowed_hosts: vec![],
            toml_as_judge_config: "./judge.toml".to_owned(),
            lock: Mutex::new(()),
        };
        Ok((Scratch(dir), editor))
    }

    #[test]
    fn no_reply_carries_a_secret() -> R {
        let (_dir, editor) = editor(
            "DATABASE_URL=postgres://u:dbpassword@localhost:5433/j\n\
             DISCORD_TOKEN=SECRETDISCORD\n\
             CFG_TEST_TOKEN=SECRETCFG\n\
             GUILD_ID=${CFG_TEST_TOKEN}\n\
             JUDGE_CONFIG=${CFG_TEST_TOKEN}\n\
             ANTHROPIC_BASE_URL=https://u:SECRETBASEPW@proxy.example\n\
             MCP_TOKEN=short-SECRETMCP\n\
             API_INTERFACES='--api --mcp'\n",
        )?;
        let draft = Draft {
            toml: TomlDraft::None,
            env: BTreeMap::from([("JUDGE_ROLE".to_owned(), "Judges".to_owned())]),
            replace: BTreeMap::new(),
        };
        let r = render(&editor, &draft)?;
        let out = report(&editor, &r).to_string();
        for secret in [
            "SECRETDISCORD",
            "SECRETCFG",
            "SECRETBASEPW",
            "SECRETMCP",
            "dbpassword",
        ] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        assert!(out.contains("<redacted>"), "{out}");
        assert!(
            out.contains("API_INTERFACES is deprecated")
                && out.contains("JUDGE_ROLES='--discord --api --mcp --jobs'"),
            "{out}"
        );
        // A hidden setting is not changed through its shown field.
        let through_field = Draft {
            toml: TomlDraft::None,
            env: BTreeMap::from([(
                "ANTHROPIC_BASE_URL".to_owned(),
                "https://x.example".to_owned(),
            )]),
            replace: BTreeMap::new(),
        };
        assert!(render(&editor, &through_field).is_err());
        Ok(())
    }

    #[test]
    fn a_replacement_is_written_and_never_echoed() -> R {
        let (_dir, editor) = editor(
            "DISCORD_TOKEN=old-token\nANTHROPIC_BASE_URL=https://u:pw@proxy.example\nJUDGE_ROLE=Judge\n",
        )?;
        let draft = |replace: &[(&str, &str)]| Draft {
            toml: TomlDraft::None,
            env: BTreeMap::new(),
            replace: replace
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        let d = draft(&[
            ("DISCORD_TOKEN", "NEWSECRETTOKEN"),
            ("ANTHROPIC_BASE_URL", "https://NEWPROXYHOST.example"),
            ("LITELLM_KEY", "NEWLITELLMKEY"),
        ]);
        let r = render(&editor, &d)?;
        let out = report(&editor, &r).to_string();
        for secret in [
            "NEWSECRETTOKEN",
            "NEWPROXYHOST",
            "NEWLITELLMKEY",
            "old-token",
        ] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        assert!(
            out.contains("DISCORD_TOKEN: new value (not shown)"),
            "{out}"
        );
        // A loader quoting a value with `{:?}` escapes it: still masked.
        let (_q, quoted) =
            self::editor("CFG_TEST_TOKEN='old\"qzx\\ord'\nJUDGE_SOURCE_URL=${CFG_TEST_TOKEN}\n")?;
        let fresh = draft(&[("CFG_TEST_TOKEN", "new\"qzxord\\secret")]);
        let out = report(&quoted, &render(&quoted, &fresh)?).to_string();
        assert!(!out.contains("qzx"), "{out}");
        let untouched = report(&quoted, &render(&quoted, &draft(&[]))?).to_string();
        assert!(!untouched.contains("qzx"), "{untouched}");
        assert!(r.env_new.contains("DISCORD_TOKEN=NEWSECRETTOKEN\n"));
        assert!(r.env_new.ends_with("LITELLM_KEY=NEWLITELLMKEY\n"));
        // A shown setting goes through its field; a bad name is refused.
        assert!(render(&editor, &draft(&[("JUDGE_ROLE", "x")])).is_err());
        assert!(render(&editor, &draft(&[("BAD NAME", "x")])).is_err());
        assert!(render(&editor, &draft(&[("DISCORD_TOKEN", " ")])).is_err());
        Ok(())
    }

    #[test]
    fn a_save_follows_a_symlink_and_keeps_the_mode() -> R {
        let (dir, editor) = editor("JUDGE_ROLE=Judge\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let real = dir.0.join("real.env");
            fs::rename(&editor.env_path, &real)?;
            fs::set_permissions(&real, fs::Permissions::from_mode(0o640))?;
            std::os::unix::fs::symlink(&real, &editor.env_path)?;
            let s = stage(&editor.env_path, "JUDGE_ROLE=Judges\n", true)?;
            fs::rename(&s.tmp, &s.target)?;
            assert!(
                fs::symlink_metadata(&editor.env_path)?
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(&real)?, "JUDGE_ROLE=Judges\n");
            assert_eq!(fs::metadata(&real)?.permissions().mode() & 0o777, 0o640);
            let fresh = dir.0.join("new.env");
            let s = stage(&fresh, "A=1\n", true)?;
            fs::rename(&s.tmp, &s.target)?;
            assert_eq!(fs::metadata(&fresh)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }
}
