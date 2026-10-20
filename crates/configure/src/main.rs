//! `judge-config` — edit `judge.toml` and `.env` in a browser.
//!
//! ```text
//! judge-config [--env FILE] [--config FILE] [--listen ADDR] [--allow-host NAME]...
//! ```
//!
//! `--env` defaults to `./.env` (created from `.env.example` on the first
//! save when missing). `--config` defaults to what that file's
//! `JUDGE_CONFIG` names, relative to its directory, else `judge.toml` beside
//! it. `--listen` defaults to `127.0.0.1:8790`. The URL printed at startup
//! carries the token the page needs; nothing is served to a browser that
//! does not have it.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use judge_configure::{
    catalog::{self, Messages},
    env::DotEnv,
    server,
};
use judge_core::Locale;
use tokio::sync::Mutex;

/// Where a container publishes the port, set by the compose service: the
/// address to print, and whether to warn about a non-loopback listener.
const PUBLISHED_ENV: &str = "JUDGE_CONFIG_PUBLISHED";

#[derive(Debug)]
struct Args {
    env: PathBuf,
    config: Option<PathBuf>,
    listen: SocketAddr,
    allow_hosts: Vec<String>,
}

fn parse(args: impl IntoIterator<Item = String>, m: &Messages) -> Result<Option<Args>> {
    let mut out = Args {
        env: PathBuf::from(".env"),
        config: None,
        listen: SocketAddr::from(([127, 0, 0, 1], 8790)),
        allow_hosts: vec![],
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .with_context(|| format!("{}\n\n{}", (m.needs_value)(&arg), m.usage))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--env" => out.env = PathBuf::from(value()?),
            "--config" => out.config = Some(PathBuf::from(value()?)),
            "--listen" => {
                out.listen = value()?.parse().context(m.bad_listen)?;
            }
            "--allow-host" => out.allow_hosts.push(value()?),
            other => anyhow::bail!("{}\n\n{}", (m.unknown_argument)(other), m.usage),
        }
    }
    Ok(Some(out))
}

/// The `judge.toml` to edit, and how `JUDGE_CONFIG` spells it: relative to
/// the `.env`'s directory (where compose resolves it) when it is inside.
/// Both are made absolute first: a relative `--config` is the working
/// directory's, a `JUDGE_CONFIG` the `.env` directory's.
fn toml_path(env: &Path, explicit: Option<PathBuf>) -> (PathBuf, String) {
    let dir = absolute(
        env.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    );
    let named = || {
        let text = std::fs::read_to_string(env).ok()?;
        let value = DotEnv::parse(&text).ok()?.lookup()(judge_bot::config::CONFIG_ENV)?;
        let value = value.trim();
        (!value.is_empty()).then(|| dir.join(value))
    };
    let path = absolute(
        &explicit
            .or_else(named)
            .unwrap_or_else(|| dir.join("judge.toml")),
    );
    let spelled = path.strip_prefix(&dir).map_or_else(
        |_| path.display().to_string(),
        |rel| format!("./{}", rel.display()),
    );
    (path, spelled)
}

/// `path` from the root, with `.` and `..` folded away lexically (the file
/// need not exist yet).
fn absolute(path: &Path) -> PathBuf {
    use std::path::Component;
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in full.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// 256 random bits as hex, from two v4 UUIDs (122 random bits each).
fn token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("judge-config: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// The terminal's language ([`Locale::for_terminal`]), else the instance's
/// as the `.env` at `env` says.
fn terminal_locale(env: &Path) -> Locale {
    let text = std::fs::read_to_string(env).ok();
    Locale::for_terminal(
        |k| std::env::var(k).ok(),
        server::instance_locale(text.as_deref()),
    )
}

async fn run() -> Result<()> {
    // The `.env` named on the command line is not known until it is parsed.
    let m = catalog::messages(terminal_locale(Path::new(".env")));
    let Some(args) = parse(std::env::args().skip(1), m)? else {
        println!("{}", m.usage);
        return Ok(());
    };
    let m = catalog::messages(terminal_locale(&args.env));
    let (toml_path, spelled) = toml_path(&args.env, args.config);
    let editor = Arc::new(server::Editor {
        env_path: absolute(&args.env),
        toml_path,
        token: token(),
        allowed_hosts: args.allow_hosts,
        toml_as_judge_config: spelled,
        lock: Mutex::new(()),
    });
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    let addr = listener.local_addr()?;
    // In a container the listener is 0.0.0.0 by necessity; the compose
    // service says where the port is really published.
    let published = std::env::var(PUBLISHED_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<SocketAddr>().ok());
    let reachable = published.unwrap_or(addr);
    if !reachable.ip().is_loopback() {
        eprintln!("{}", (m.not_loopback)(&addr.to_string(), addr.port()));
    }
    let shown = if reachable.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], reachable.port()))
    } else {
        reachable
    };
    println!(
        "{}",
        (m.editing)(
            &editor.env_path.display().to_string(),
            &editor.toml_path.display().to_string()
        )
    );
    println!(
        "{}",
        (m.open)(&format!("http://{shown}/#token={}", editor.token))
    );
    println!("{}", m.stop);
    axum::serve(listener, server::router(editor))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("serving")
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn arguments() -> R {
        let m = &catalog::EN;
        let a = parse(
            [
                "--env",
                "x/.env",
                "--listen",
                "0.0.0.0:9000",
                "--allow-host",
                "box",
            ]
            .map(String::from),
            m,
        )?
        .ok_or("help")?;
        assert_eq!(a.env, PathBuf::from("x/.env"));
        assert_eq!(a.listen.port(), 9000);
        assert_eq!(a.allow_hosts, ["box"]);
        assert!(parse(["--help".to_owned()], m)?.is_none());
        assert!(parse(["--env".to_owned()], m).is_err());
        assert!(parse(["--nope".to_owned()], m).is_err());
        // An argument error is in the terminal's language, with the usage.
        let stub = catalog::messages(Locale::Stub);
        let e = parse(["--nope".to_owned()], stub).err().ok_or("refused")?;
        assert_eq!(format!("{e:#}"), "⟦unknown_argument --nope⟧\n\n⟦usage⟧");
        Ok(())
    }

    #[test]
    fn the_toml_beside_the_env_is_spelled_relative() {
        let (path, spelled) = toml_path(Path::new("deploy/.env-does-not-exist"), None);
        assert!(
            path.is_absolute() && path.ends_with("deploy/judge.toml"),
            "{path:?}"
        );
        assert_eq!(spelled, "./judge.toml");
        let (_, spelled) = toml_path(Path::new(".env-nope"), Some(PathBuf::from("/etc/j.toml")));
        assert_eq!(spelled, "/etc/j.toml");
        // An absolute path inside the default directory is still relative.
        let inside = absolute(Path::new("x/../judge.toml"));
        let (_, spelled) = toml_path(Path::new(".env-nope"), Some(inside));
        assert_eq!(spelled, "./judge.toml");
        // A relative --config is the working directory's, not the .env's.
        let (_, spelled) = toml_path(Path::new("a/.env-nope"), Some(PathBuf::from("judge.toml")));
        assert!(spelled.starts_with('/'), "{spelled}");
    }
}
