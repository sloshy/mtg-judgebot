//! Upload Magic's card symbols to Discord as *application* emoji, so the bot
//! can render `{W}` as a picture instead of as text.
//!
//! Application emoji belong to the bot's application rather than to a server:
//! they work in every guild it posts in, need no emoji slots, and Discord
//! allows up to 2000 (Scryfall publishes 84). The bot reads them back at
//! startup and builds its `SymbolTable` from the names, so this uploader and
//! the renderer agree by sharing one `emoji_name` — there is no list of
//! symbols hard-coded on either side.
//!
//! Scryfall serves the symbols as SVG and Discord only takes raster images, so
//! each one is rendered to a [`PNG_SIZE`]-pixel PNG on the way through.
//!
//! The run is idempotent: symbols whose emoji already exist are left alone, so
//! re-running after Scryfall adds a symbol uploads only the new one.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use judge_core::symbol;
use serde::Deserialize;
use serenity::http::Http;

/// Every card symbol Magic uses, with a link to its SVG.
const SYMBOLOGY_URL: &str = "https://api.scryfall.com/symbology";
/// Scryfall asks every client to identify itself.
const USER_AGENT: &str = concat!(
    "mtg-judgebot-ingest/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/sloshy/mtg-judgebot)"
);
/// resvg is built without its text, font and raster-image features: none of
/// Scryfall's 84 symbols uses `<text>`, `<image>`, `<use>` or a `<style>`
/// block, so there is nothing to lose by leaving them out. `svgz` goes too,
/// which is fine while Scryfall serves plain `.svg`.
///
/// Scryfall asks for 50–100 ms between requests.
const SCRYFALL_DELAY: Duration = Duration::from_millis(100);
/// Breathing room between uploads, on top of serenity's rate limiter.
const UPLOAD_DELAY: Duration = Duration::from_millis(250);
/// Pixels per side of the uploaded PNG. Discord displays emoji far smaller and
/// caps the upload at 256 KB; these symbols come out around 2–6 KB.
const PNG_SIZE: u32 = 128;
/// Discord's limit on an emoji image, in bytes.
const MAX_IMAGE_BYTES: usize = 256 * 1024;

/// One entry of `GET /symbology`.
#[derive(Debug, Deserialize)]
struct Symbol {
    /// The symbol as it appears in Oracle text, braces included: `{W/U}`.
    symbol: String,
    svg_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Symbology {
    data: Vec<Symbol>,
}

/// What a run did, so the caller can log one line and tests can assert on it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Emoji created on this run.
    pub uploaded: usize,
    /// Symbols whose emoji already existed.
    pub skipped: usize,
    /// Symbols Scryfall published that this tool cannot name, or that have no
    /// SVG to draw. A non-zero count means the naming rule needs extending.
    pub unusable: usize,
    /// Symbols that could not be fetched, drawn or uploaded this run. They are
    /// simply missing until the next run; nothing else is affected.
    pub failed: usize,
}

/// Strip the braces Scryfall writes around a symbol: `{W/U}` → `W/U`.
/// `None` if it is not brace-wrapped, which would mean the API changed shape.
fn body(symbol: &str) -> Option<&str> {
    symbol.strip_prefix('{')?.strip_suffix('}')
}

/// Draw an SVG into a square bitmap of `size` pixels a side, scaled to fit and
/// **centred**.
///
/// Discord emoji are square, and nine of Scryfall's 84 symbols are not:
/// `{1000000}` is a 76×15 strip, `{TK}` is 539×696, `{PW}` and the half-mana
/// symbols are tall. Scaling on the longer side alone would pin those to the
/// top-left corner, which at Discord's ~22 px display size shows as a smear
/// against the edge; the translate puts them in the middle instead.
fn draw(svg: &[u8], size: u32) -> Result<resvg::tiny_skia::Pixmap> {
    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())
        .context("parsing the SVG")?;
    let source = tree.size();
    anyhow::ensure!(
        source.width() > 0.0 && source.height() > 0.0,
        "the SVG has no area"
    );
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size, size).context("allocating the target bitmap")?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "`size` is an emoji edge in pixels; f32 is exact well past any sane value"
    )]
    let edge = size as f32;
    let scale = edge / source.width().max(source.height());
    let transform = resvg::tiny_skia::Transform::from_translate(
        (edge - source.width() * scale) / 2.0,
        (edge - source.height() * scale) / 2.0,
    )
    .pre_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

/// [`draw`] encoded as the PNG Discord is sent.
fn rasterize(svg: &[u8], size: u32) -> Result<Vec<u8>> {
    draw(svg, size)?.encode_png().context("encoding the PNG")
}

/// Fetch `url` over the network, unconditionally.
async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    tokio::time::sleep(SCRYFALL_DELAY).await;
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?
        .to_vec();
    Ok(bytes)
}

/// Fetch `url`, caching the bytes at `path` so a re-run costs Scryfall nothing.
///
/// Only for immutable content. The symbology *index* is deliberately not cached
/// through here: a frozen copy would mean a re-run could never see a symbol
/// Scryfall has added, which is the one reason to re-run at all.
async fn cached_get(client: &reqwest::Client, url: &str, path: &Path) -> Result<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(path)
        && !bytes.is_empty()
    {
        return Ok(bytes);
    }
    let bytes = get(client, url).await?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A partial write must not become a poisoned cache entry. The suffix is
    // appended rather than replacing the extension (as `with_extension` would),
    // so two cache entries can never share a temporary path.
    let tmp = path.with_file_name(format!(
        "{}.part",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(bytes)
}

/// An authenticated Discord client that knows its own application id, which
/// the emoji routes need and `Http::new` cannot know on its own.
async fn discord(token: &str) -> Result<Http> {
    let http = Http::new(token);
    let app = http
        .get_current_application_info()
        .await
        .context("asking Discord which application this token belongs to")?;
    tracing::info!(application = %app.name, id = %app.id, "authenticated");
    http.set_application_id(app.id);
    Ok(http)
}

/// Upload every Scryfall symbol that this application does not already have.
///
/// # Errors
/// A missing `DISCORD_TOKEN`, an unusable token, an unreachable Scryfall or
/// Discord, or two symbols wanting the same emoji name. A single symbol that
/// cannot be fetched, drawn or uploaded is counted in [`Summary::failed`] and
/// the run carries on; the summary is logged either way, so a failure never
/// hides the emoji that were already created.
pub async fn run(cache_dir: &Path) -> Result<Summary> {
    let token = std::env::var("DISCORD_TOKEN")
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .context("DISCORD_TOKEN is not set: the emoji belong to the bot's own application")?;
    let http = discord(&token).await?;
    let mut existing: HashSet<String> = http
        .get_application_emojis()
        .await
        .context("listing the application's emoji")?
        .into_iter()
        .map(|e| e.name)
        .collect();
    tracing::info!(existing = existing.len(), "current application emoji");

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("building the HTTP client")?;
    // Never cached: a frozen index could not show a symbol Scryfall has added,
    // which is the whole reason to run this again.
    let index = get(&client, SYMBOLOGY_URL).await?;
    let symbology: Symbology =
        serde_json::from_slice(&index).context("parsing Scryfall's symbology")?;
    tracing::info!(symbols = symbology.data.len(), "fetched symbology");

    let mut summary = Summary::default();
    let plan = plan(&symbology.data, &mut summary)?;
    let dir: PathBuf = cache_dir.join("symbols");
    for (name, symbol, svg_uri) in plan {
        if existing.contains(&name) {
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        }
        match upload(&http, &client, &dir, &name, &svg_uri).await {
            Ok(bytes) => {
                tracing::info!(%symbol, %name, bytes, "uploaded");
                // So a later symbol that somehow shares the name cannot
                // overwrite this one, and a retry within the run is a no-op.
                existing.insert(name);
                summary.uploaded = summary.uploaded.saturating_add(1);
                tokio::time::sleep(UPLOAD_DELAY).await;
            }
            Err(e) => {
                tracing::warn!(%symbol, %name, error = format_args!("{e:#}"), "could not upload; skipping");
                summary.failed = summary.failed.saturating_add(1);
            }
        }
    }
    tracing::info!(?summary, "emoji sync complete");
    Ok(summary)
}

/// What to upload, as `(emoji name, symbol, svg uri)`.
///
/// # Errors
/// Two symbols wanting one emoji name. [`emoji_name`](symbol::emoji_name) is
/// injective over everything Scryfall publishes today (there is a test in
/// `judge-core` pinning that), so this can only fire on a symbol they add whose
/// name collides — and it is far better to refuse before uploading anything
/// than to have one symbol silently overwrite another's picture.
fn plan(symbols: &[Symbol], summary: &mut Summary) -> Result<Vec<(String, String, String)>> {
    let mut plan: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for s in symbols {
        let Some((name, svg_uri)) = body(&s.symbol)
            .and_then(symbol::emoji_name)
            .zip(s.svg_uri.as_deref())
        else {
            tracing::warn!(symbol = %s.symbol, "no emoji name or no SVG; skipping");
            summary.unusable = summary.unusable.saturating_add(1);
            continue;
        };
        anyhow::ensure!(
            seen.insert(name.clone()),
            "{} and an earlier symbol both want the emoji name {name}",
            s.symbol
        );
        plan.push((name, s.symbol.clone(), svg_uri.to_owned()));
    }
    Ok(plan)
}

/// Fetch, draw and upload one symbol; the PNG's size on success.
async fn upload(
    http: &Http,
    client: &reqwest::Client,
    dir: &Path,
    name: &str,
    svg_uri: &str,
) -> Result<usize> {
    let svg = cached_get(client, svg_uri, &dir.join(format!("{name}.svg"))).await?;
    let png = rasterize(&svg, PNG_SIZE)?;
    anyhow::ensure!(
        png.len() <= MAX_IMAGE_BYTES,
        "rendered to {} bytes, over Discord's {MAX_IMAGE_BYTES}-byte limit",
        png.len()
    );
    let image = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    http.create_application_emoji(&serde_json::json!({ "name": name, "image": image }))
        .await
        .context("creating the emoji")?;
    Ok(png.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braces_are_stripped_and_bad_shapes_rejected() {
        assert_eq!(body("{W/U}"), Some("W/U"));
        assert_eq!(body("{T}"), Some("T"));
        assert_eq!(body("W"), None);
        assert_eq!(body("{W"), None);
        assert_eq!(body("W}"), None);
    }

    /// A square symbol, shaped like Scryfall's: a group transform with
    /// `fill='none'` inherited by the shapes inside it.
    const SQUARE_SVG: &[u8] = br"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><g transform='translate(0 -1)' fill='none'><circle fill='#CAC5C0' cx='50' cy='50.998' r='50'/><path d='M85 60H49l13-9z' fill='#000'/></g></svg>";

    /// The shape of `{1000000}`: a wide strip that must end up centred rather
    /// than smeared along the top edge.
    const WIDE_SVG: &[u8] = br"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 76 15'><rect x='0' y='0' width='76' height='15' fill='#000'/></svg>";

    /// Rows of the rendered bitmap that contain any non-transparent pixel.
    fn painted_rows(pixmap: &resvg::tiny_skia::Pixmap) -> Vec<u32> {
        (0..pixmap.height())
            .filter(|y| {
                (0..pixmap.width()).any(|x| pixmap.pixel(x, *y).is_some_and(|p| p.alpha() > 0))
            })
            .collect()
    }

    #[test]
    fn a_square_symbol_fills_the_bitmap() -> Result<()> {
        let pixmap = draw(SQUARE_SVG, PNG_SIZE)?;
        let rows = painted_rows(&pixmap);
        // Something was actually drawn — a blank bitmap still encodes to a
        // perfectly valid ~143-byte PNG, so size alone proves nothing.
        assert!(!rows.is_empty(), "nothing was drawn");
        assert_eq!(rows.first().copied(), Some(0), "{rows:?}");
        assert_eq!(
            rows.last().copied(),
            Some(PNG_SIZE - 1),
            "a square symbol should reach both edges"
        );
        Ok(())
    }

    /// The bug this catches: scaling without a translate pinned every
    /// non-square symbol to the top-left corner, so `{1000000}` displayed as a
    /// smear against the top edge of its emoji box.
    #[test]
    fn a_wide_symbol_is_centred_not_pinned_to_the_top() -> Result<()> {
        let pixmap = draw(WIDE_SVG, PNG_SIZE)?;
        let rows = painted_rows(&pixmap);
        let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
            return Err(anyhow::anyhow!("nothing was drawn"));
        };
        // 76x15 scaled to fit 128 wide is ~25 tall, so ~51 blank rows above
        // and below. Equal margins are the whole point.
        assert!(first > 40, "top margin is only {first} rows: not centred");
        let bottom = PNG_SIZE - 1 - last;
        assert!(
            first.abs_diff(bottom) <= 1,
            "margins {first} above and {bottom} below are not equal"
        );
        Ok(())
    }

    #[test]
    fn a_rendered_symbol_is_a_png_discord_will_take() -> Result<()> {
        let png = rasterize(SQUARE_SVG, PNG_SIZE)?;
        assert_eq!(png.get(..4), Some(b"\x89PNG".as_slice()));
        assert!(png.len() <= MAX_IMAGE_BYTES, "{} bytes", png.len());
        Ok(())
    }

    #[test]
    fn a_broken_svg_is_an_error_not_a_panic() {
        assert!(rasterize(b"not an svg at all", PNG_SIZE).is_err());
        assert!(rasterize(b"", PNG_SIZE).is_err());
    }

    fn symbol(s: &str, uri: Option<&str>) -> Symbol {
        Symbol {
            symbol: s.to_owned(),
            svg_uri: uri.map(str::to_owned),
        }
    }

    #[test]
    fn planning_names_every_symbol_and_counts_the_unusable() -> Result<()> {
        let mut summary = Summary::default();
        let plan = plan(
            &[
                symbol("{W}", Some("https://example.test/W.svg")),
                symbol("{W/U}", Some("https://example.test/WU.svg")),
                // No SVG to draw, and a body that cannot be named.
                symbol("{G}", None),
                symbol("{a b}", Some("https://example.test/x.svg")),
            ],
            &mut summary,
        )?;
        assert_eq!(
            plan.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(),
            ["mana_w", "mana_wu"]
        );
        assert_eq!(summary.unusable, 2);
        Ok(())
    }

    /// Two symbols wanting one name must stop the run before anything is
    /// uploaded, rather than let the second silently overwrite the first.
    #[test]
    fn planning_refuses_a_name_collision() {
        let mut summary = Summary::default();
        let err = plan(
            &[
                symbol("{W/U}", Some("https://example.test/a.svg")),
                symbol("{WU}", Some("https://example.test/b.svg")),
            ],
            &mut summary,
        )
        .err()
        .map(|e| e.to_string());
        assert!(
            err.as_ref().is_some_and(|m| m.contains("mana_wu")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_cached_file_is_returned_without_a_request() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("judgebot-emoji-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("mana_w.svg");
        std::fs::write(&path, b"<svg/>")?;
        // Short timeout: the miss path must fail fast, not hang the suite.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(200))
            .timeout(Duration::from_millis(500))
            .build()?;
        // The URL is unroutable: reaching the network at all would fail.
        let got = cached_get(&client, "http://127.0.0.1:1/nope", &path).await?;
        assert_eq!(got, b"<svg/>");
        // An empty cache entry does not count as a hit.
        std::fs::write(&path, b"")?;
        assert!(
            cached_get(&client, "http://127.0.0.1:1/nope", &path)
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
