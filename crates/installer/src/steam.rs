//! Steam client integration over the CEF remote-debugging endpoint.
//!
//! In Developer Mode the Steam client on the headset exposes Chrome DevTools
//! on `127.0.0.1:8080` (Frame Control uses it over an SSH tunnel). Its
//! `SharedJSContext` page has the `SteamClient` JS API that Steam's own UI
//! uses, which lets us create the library entry, set artwork, add it to
//! Favorites and launch it, without touching `shortcuts.vdf` while Steam runs.
//!
//! [verify] Everything in this module against the Frame's Steam client: the
//! port, the target title, and the `SteamClient.Apps.*` method names and
//! signatures (taken from Decky Loader / SteamGridDB plugin usage on the
//! Deck), the artwork asset-type numbers, and the favourites API.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// CEF remote debugging port on the headset.
pub const CEF_DEBUG_PORT: u16 = 8080;
/// DevTools target that hosts the `SteamClient` API.
pub const SHARED_CONTEXT_TITLE: &str = "SharedJSContext";

/// Steam custom-artwork slots (`SetCustomArtworkForApp` asset types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtKind {
    /// Portrait library capsule (600×900).
    Grid = 0,
    /// Library hero banner (3840×1240).
    Hero = 1,
    /// Logo drawn over the hero (transparent PNG).
    Logo = 2,
    /// Landscape capsule (920×430).
    GridHorizontal = 3,
    /// Small icon.
    Icon = 4,
}

impl ArtKind {
    pub const ALL: [ArtKind; 5] = [
        Self::Grid,
        Self::Hero,
        Self::Logo,
        Self::GridHorizontal,
        Self::Icon,
    ];

    /// File stem used in `share/steam/<stem>.png` inside release tarballs.
    pub fn file_stem(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Hero => "hero",
            Self::Logo => "logo",
            Self::GridHorizontal => "grid_horizontal",
            Self::Icon => "icon",
        }
    }
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

/// JS: resolve the appid of our shortcut, or `null`. An exact name match
/// wins; otherwise the first shortcut whose exe path contains `exe_hint` and
/// not `exclude` (so "FramePlayer" never resolves to the self-test entry,
/// whose exe lives in the same directory).
pub fn js_find_shortcut(name: &str, exe_hint: &str, exclude: &str) -> String {
    format!(
        r#"(async () => {{
  const name = {n}, hint = {h}, ex = {x};
  try {{
    const all = (await SteamClient.Apps.GetAllShortcuts()).filter(s => s.data);
    let m = all.find(s => s.data.strAppName === name);
    if (!m) m = all.find(s => {{ const p = s.data.strExePath || ""; return p.includes(hint) && !(ex && p.includes(ex)); }});
    if (m) return m.appid;
  }} catch (e) {{}}
  try {{
    const a = window.appStore && appStore.allApps.find(a => a.display_name === name);
    if (a) return a.appid;
  }} catch (e) {{}}
  return null;
}})()"#,
        n = js_str(name),
        h = js_str(exe_hint),
        x = js_str(exclude)
    )
}

/// JS: create a non-Steam shortcut and return its appid.
pub fn js_add_shortcut(name: &str, exe: &str, start_dir: &str, launch_options: &str) -> String {
    format!(
        r#"(async () => {{
  const id = await SteamClient.Apps.AddShortcut({n}, {e}, {d}, {o});
  try {{ SteamClient.Apps.SetShortcutName(id, {n}); }} catch (e) {{}}
  return id;
}})()"#,
        n = js_str(name),
        e = js_str(exe),
        d = js_str(start_dir),
        o = js_str(launch_options)
    )
}

/// JS: set one artwork slot from PNG bytes.
pub fn js_set_artwork(appid: u32, kind: ArtKind, png: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "(async () => {{ await SteamClient.Apps.SetCustomArtworkForApp({appid}, {}, \"png\", {}); return true; }})()",
        js_str(&b64),
        kind as u32
    )
}

/// JS: add the app to the Favorites collection (shown on the home screen).
pub fn js_pin(appid: u32) -> String {
    format!(
        r#"(async () => {{
  const id = {appid};
  if (window.collectionStore) {{
    if (typeof collectionStore.SetAppsAsFavorite === "function") {{ collectionStore.SetAppsAsFavorite([id], true); return "favorite"; }}
    const fav = collectionStore.GetCollection && collectionStore.GetCollection("favorite");
    const ov = window.appStore && appStore.GetAppOverviewByAppID(id);
    if (fav && ov && fav.AsDragDropCollection) {{ fav.AsDragDropCollection().AddApps([ov]); return "favorite"; }}
  }}
  throw new Error("no favourites API found");
}})()"#
    )
}

/// 64-bit game id Steam uses to launch a non-Steam shortcut.
pub fn shortcut_game_id(appid: u32) -> u64 {
    ((appid as u64) << 32) | 0x0200_0000
}

/// JS: launch the shortcut.
pub fn js_run(appid: u32) -> String {
    format!(
        "(async () => {{ SteamClient.Apps.RunGame({}, \"\", -1, 100); return true; }})()",
        js_str(&shortcut_game_id(appid).to_string())
    )
}

/// JS: remove the shortcut.
pub fn js_remove(appid: u32) -> String {
    format!("(async () => {{ SteamClient.Apps.RemoveShortcut({appid}); return true; }})()")
}

/// Pick the DevTools websocket URL from `/json`, rewritten to our tunnel.
pub fn pick_target(targets: &Value, local_port: u16) -> Option<String> {
    let list = targets.as_array()?;
    let ws = |t: &Value| t.get("webSocketDebuggerUrl")?.as_str().map(String::from);
    let title = |t: &Value| {
        t.get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let chosen = list
        .iter()
        .find(|t| title(t) == SHARED_CONTEXT_TITLE)
        .or_else(|| {
            list.iter()
                .find(|t| title(t).contains("Steam") && ws(t).is_some())
        })?;
    let url = ws(chosen)?;
    // ws://127.0.0.1:8080/devtools/page/<id> → our forwarded port.
    let path = url.splitn(4, '/').nth(3)?;
    Some(format!("ws://127.0.0.1:{local_port}/{path}"))
}

/// Extract the value (or the exception) from a `Runtime.evaluate` reply.
pub fn eval_result(reply: &Value) -> Result<Value> {
    if let Some(err) = reply.get("error") {
        bail!("DevTools error: {err}");
    }
    let r = reply
        .get("result")
        .ok_or_else(|| anyhow!("malformed DevTools reply"))?;
    if let Some(ex) = r.get("exceptionDetails") {
        let msg = ex
            .pointer("/exception/description")
            .or_else(|| ex.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("exception");
        bail!("Steam client script failed: {msg}");
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A DevTools session with the Steam client's shared JS context.
pub struct SteamCdp {
    ws: Ws,
    next_id: u64,
}

impl SteamCdp {
    /// Connect through a local port forwarded to the headset's port 8080.
    pub async fn connect(local_port: u16) -> Result<Self> {
        let list: Value = reqwest::get(format!("http://127.0.0.1:{local_port}/json"))
            .await
            .context(
                "Steam DevTools endpoint not reachable (is Steam running and Developer Mode on?)",
            )?
            .json()
            .await?;
        let url = pick_target(&list, local_port)
            .ok_or_else(|| anyhow!("Steam's {SHARED_CONTEXT_TITLE} not found"))?;
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .context("DevTools websocket")?;
        Ok(Self { ws, next_id: 1 })
    }

    /// Evaluate an expression (awaiting promises) and return its JSON value.
    pub async fn eval(&mut self, expression: &str) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": { "expression": expression, "awaitPromise": true, "returnByValue": true }
        });
        self.ws.send(Message::Text(msg.to_string())).await?;
        let wait = async {
            while let Some(m) = self.ws.next().await {
                if let Message::Text(t) = m? {
                    let v: Value = serde_json::from_str(&t)?;
                    if v.get("id").and_then(Value::as_u64) == Some(id) {
                        return eval_result(&v);
                    }
                }
            }
            bail!("DevTools connection closed")
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), wait)
            .await
            .map_err(|_| anyhow!("Steam client did not answer within 30 s"))?
    }

    pub async fn find_shortcut(
        &mut self,
        name: &str,
        exe_hint: &str,
        exclude: &str,
    ) -> Result<Option<u32>> {
        let v = self
            .eval(&js_find_shortcut(name, exe_hint, exclude))
            .await?;
        Ok(v.as_u64().map(|x| x as u32))
    }

    pub async fn add_shortcut(&mut self, name: &str, exe: &str, start_dir: &str) -> Result<u32> {
        let v = self
            .eval(&js_add_shortcut(name, exe, start_dir, ""))
            .await?;
        v.as_u64()
            .map(|x| x as u32)
            .ok_or_else(|| anyhow!("AddShortcut returned {v}"))
    }

    pub async fn set_artwork(&mut self, appid: u32, kind: ArtKind, png: &[u8]) -> Result<()> {
        self.eval(&js_set_artwork(appid, kind, png))
            .await
            .map(|_| ())
    }

    pub async fn pin(&mut self, appid: u32) -> Result<()> {
        self.eval(&js_pin(appid)).await.map(|_| ())
    }

    pub async fn run(&mut self, appid: u32) -> Result<()> {
        self.eval(&js_run(appid)).await.map(|_| ())
    }

    pub async fn remove(&mut self, appid: u32) -> Result<()> {
        self.eval(&js_remove(appid)).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_strings_are_escaped() {
        let js = js_find_shortcut("Frame\"Player</script>", "/x", "probe");
        assert!(js.contains(r#""Frame\"Player</script>""#));
        assert!(js.contains(r#"ex = "probe""#));
        let js = js_add_shortcut(
            "FramePlayer",
            "/home/steamos/devkit-game/frameplayer/frameplayer.sh",
            "/d",
            "",
        );
        assert!(js.contains("SteamClient.Apps.AddShortcut(\"FramePlayer\", \"/home/steamos/devkit-game/frameplayer/frameplayer.sh\", \"/d\", \"\")"));
    }

    #[test]
    fn artwork_payload() {
        let js = js_set_artwork(123, ArtKind::Hero, b"\x89PNG");
        assert!(js.contains("SetCustomArtworkForApp(123, \"iVBORw==\", \"png\", 1)"));
        assert_eq!(ArtKind::GridHorizontal.file_stem(), "grid_horizontal");
    }

    #[test]
    fn game_id_for_shortcut() {
        assert_eq!(shortcut_game_id(0x8000_0001), 0x8000_0001_0200_0000);
        assert!(js_run(1).contains("\"4328521728\""));
    }

    #[test]
    fn target_selection() {
        let list = json!([
            {"title": "Steam Big Picture Mode", "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/AAA"},
            {"title": "SharedJSContext", "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/BBB"}
        ]);
        assert_eq!(
            pick_target(&list, 41000).unwrap(),
            "ws://127.0.0.1:41000/devtools/page/BBB"
        );
        let fallback = json!([{"title": "Steam", "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/C"}]);
        assert_eq!(
            pick_target(&fallback, 1).unwrap(),
            "ws://127.0.0.1:1/devtools/page/C"
        );
        assert!(pick_target(&json!([{"title": "other"}]), 1).is_none());
    }

    #[test]
    fn eval_results() {
        let ok = json!({"id": 1, "result": {"result": {"type": "number", "value": 42}}});
        assert_eq!(eval_result(&ok).unwrap(), 42);
        let ex = json!({"id": 1, "result": {"result": {}, "exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x"}}}});
        assert!(eval_result(&ex)
            .unwrap_err()
            .to_string()
            .contains("TypeError"));
        assert!(eval_result(&json!({"id": 1, "error": {"code": -1}})).is_err());
        let undef = json!({"id": 1, "result": {"result": {"type": "undefined"}}});
        assert_eq!(eval_result(&undef).unwrap(), Value::Null);
    }
}
