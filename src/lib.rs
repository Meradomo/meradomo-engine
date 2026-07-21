//! # meradomo-engine — the reusable launcher for a shared Meradomo engine
//!
//! An embedding app uses this crate to serve its local app through Meradomo
//! without a second download. It handles the whole shared-engine dance:
//!
//! 1. **discover** a Meradomo engine already running on this machine
//!    ([`discover`]) and decide whether to attach to it or start its own
//!    ([`decide_start_action`]);
//! 2. **spawn** the bundled engine ([`EngineConfig::spawn`]) or **attach** to a
//!    running one ([`spawn_or_attach`]);
//! 3. **register** itself as a live consumer so the engine's reference-counted
//!    lifecycle keeps serving while the app is open ([`register`], [`heartbeat`],
//!    [`deregister`]);
//! 4. **publish** its local app to a public address ([`publish`], [`unpublish`],
//!    [`status`]).
//!
//! It is **Tauri-agnostic**: it takes plain config and returns
//! [`std::process::Child`] / typed results, so any host — a Tauri app, a CLI, a
//! service — can drive it. The transport is HTTP over loopback to the engine's
//! management endpoint (default `http://127.0.0.1:8765`).
//!
//! ## The raw wire protocol (for non-Rust hosts)
//!
//! All calls are plain HTTP to the management base URL; no auth for the engine
//! surface (loopback + same-user is the trust boundary). JSON bodies use
//! camelCase keys.
//!
//! | Method & path              | Body                          | Purpose |
//! |----------------------------|-------------------------------|---------|
//! | `GET  /engine/info`        | —                             | discovery: `{engineVersion, protocol, pid, mode, connected, host, name, firstPartyApp, registrants}` |
//! | `POST /engine/register`    | `{appId, pid}`                | attach as a live consumer |
//! | `POST /engine/heartbeat`   | `{appId, pid}`                | stay attached (idempotently registers) |
//! | `POST /engine/deregister`  | `{appId}`                     | detach (last one out stops the engine) |
//! | `POST /publish`            | `{name, label, localPort, appId?}` | request a public address (first-party `appId` auto-approves) |
//! | `GET  /publish/:name`      | —                             | poll publish status |
//! | `DELETE /publish/:name`    | —                             | unpublish (keeps approval) |
//! | `GET  /status`             | —                             | connection state + published apps |
//!
//! A host attaches to an incumbent only when its `protocol` matches
//! [`ENGINE_PROTOCOL`]; a different number means "incompatible — start your own".

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Wire protocol version this crate speaks. Must match `ENGINE_PROTOCOL` in the
/// agent (`agent/src/engine-registry.js`). Bump together on any breaking change.
pub const ENGINE_PROTOCOL: u32 = 1;

/// The engine's default management base URL (loopback only).
pub const DEFAULT_MGMT_BASE: &str = "http://127.0.0.1:8765";

const CALL_TIMEOUT: Duration = Duration::from_millis(1500);

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::new()
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The account's billing standing (P2.4), surfaced so an embed can show a clear
/// "renew to keep serving" state instead of a silent route-down.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BillingInfo {
    #[serde(default)]
    pub entitled: bool,
    /// "comp" | "active" | "trialing" | "past_due" | "hold"
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub trial_ends_at: Option<i64>,
}

/// The `GET /engine/info` discovery shape.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EngineInfo {
    #[serde(default)]
    pub engine_version: String,
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub first_party_app: Option<String>,
    #[serde(default)]
    pub registrants: u32,
    #[serde(default)]
    pub billing: Option<BillingInfo>,
}

impl EngineInfo {
    /// True when the subscription has lapsed and the person must renew to keep
    /// serving. An embed maps this to a plain "renew to keep serving" prompt.
    pub fn needs_renewal(&self) -> bool {
        matches!(
            self.billing.as_ref().map(|b| b.status.as_str()),
            Some("hold") | Some("past_due")
        )
    }
}

/// Probe for a Meradomo engine on `mgmt_base`. Returns `None` if nothing answers,
/// the answer is not an engine, or the request fails.
pub fn discover(mgmt_base: &str) -> Option<EngineInfo> {
    client()
        .get(format!("{mgmt_base}/engine/info"))
        .timeout(CALL_TIMEOUT)
        .send()
        .ok()?
        .json::<EngineInfo>()
        .ok()
}

/// What a launching app should do when it finds the port already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    /// A healthy, protocol-compatible engine is running — attach to it.
    Attach,
    /// No/failed answer or an incompatible protocol — start your own engine.
    Takeover,
}

/// Decide attach-vs-takeover from a discovery result. Mirrors the agent's
/// `decideStartAction`: a compatible engine → attach; anything else → takeover.
pub fn decide_start_action(info: Option<&EngineInfo>, protocol: u32) -> StartAction {
    match info {
        Some(i) if i.protocol == protocol => StartAction::Attach,
        _ => StartAction::Takeover,
    }
}

// ---------------------------------------------------------------------------
// Registration (reference-counted lifecycle)
// ---------------------------------------------------------------------------

/// Attach this app to the engine so its lifecycle counts us as alive.
pub fn register(mgmt_base: &str, app_id: &str, pid: u32) -> reqwest::Result<()> {
    client()
        .post(format!("{mgmt_base}/engine/register"))
        .json(&serde_json::json!({ "appId": app_id, "pid": pid }))
        .timeout(CALL_TIMEOUT)
        .send()
        .map(|_| ())
}

/// Keep this app's registration fresh (idempotently registers if unknown).
/// Best-effort: a failure (engine still coming up) is silently ignored.
pub fn heartbeat(mgmt_base: &str, app_id: &str, pid: u32) {
    let _ = client()
        .post(format!("{mgmt_base}/engine/heartbeat"))
        .json(&serde_json::json!({ "appId": app_id, "pid": pid }))
        .timeout(CALL_TIMEOUT)
        .send();
}

/// Detach this app. When it was the last registrant the engine stops serving
/// after its grace window. Best-effort — the engine also reaps a dead pid.
pub fn deregister(mgmt_base: &str, app_id: &str) {
    let _ = client()
        .post(format!("{mgmt_base}/engine/deregister"))
        .json(&serde_json::json!({ "appId": app_id }))
        .timeout(CALL_TIMEOUT)
        .send();
}

// ---------------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------------

/// The `POST /publish` / `GET /publish/:name` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Request a public address for a local app. When `app_id` matches the engine's
/// configured first-party app the route goes live immediately; otherwise it is
/// `pending` until the owner approves it.
pub fn publish(
    mgmt_base: &str,
    name: &str,
    label: &str,
    local_port: u16,
    app_id: Option<&str>,
) -> reqwest::Result<PublishResult> {
    let mut body = serde_json::json!({ "name": name, "label": label, "localPort": local_port });
    if let Some(id) = app_id {
        body["appId"] = serde_json::Value::String(id.to_string());
    }
    client()
        .post(format!("{mgmt_base}/publish"))
        .json(&body)
        .timeout(CALL_TIMEOUT)
        .send()?
        .json::<PublishResult>()
}

/// Poll the current publish status of a named app.
pub fn publish_status(mgmt_base: &str, name: &str) -> reqwest::Result<PublishResult> {
    client()
        .get(format!("{mgmt_base}/publish/{name}"))
        .timeout(CALL_TIMEOUT)
        .send()?
        .json::<PublishResult>()
}

/// Remove a live route but keep the owner's approval on record.
pub fn unpublish(mgmt_base: &str, name: &str) {
    let _ = client()
        .delete(format!("{mgmt_base}/publish/{name}"))
        .timeout(CALL_TIMEOUT)
        .send();
}

/// The engine's `GET /status` (connection state + published apps).
pub fn status(mgmt_base: &str) -> Option<serde_json::Value> {
    client()
        .get(format!("{mgmt_base}/status"))
        .timeout(CALL_TIMEOUT)
        .send()
        .ok()?
        .json::<serde_json::Value>()
        .ok()
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Everything needed to launch a bundled engine. The host resolves the paths
/// (from its Tauri resources / sidecars) and the credential, then hands them off.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Program to run (the bundled Node runtime, or `"node"` in dev).
    pub node_bin: PathBuf,
    /// The bundled `agent.mjs`.
    pub agent_path: PathBuf,
    /// `--mode` (usually `"portal"`).
    pub mode: String,
    /// Per-device credential (`--device-token`).
    pub device_token: String,
    /// `--relay-addr` (may be `host` or `host:port`).
    pub relay_addr: String,
    /// `--control-plane` URL.
    pub control_plane: String,
    /// `--local-port` the agent serves on (default 8443).
    pub local_port: u16,
    /// `--mgmt-secret` for the owner tier.
    pub mgmt_secret: String,
    /// `--frp-token` (omitted when empty).
    pub frp_token: Option<String>,
    /// `--frpc-bin` — the pinned sidecar (omitted in dev / with an override).
    pub frpc_bin: Option<PathBuf>,
    /// `--cert-mode` (`acme` in release; None keeps the agent's `static` default).
    pub cert_mode: Option<String>,
    /// `--first-party-app` — auto-approve this app's own publish (embeds only).
    pub first_party_app: Option<String>,
    /// `--engine-version` — stamp reported by `/engine/info` (from bundle.json).
    pub engine_version: Option<String>,
    /// `--work-dir` — private state dir (None = platform default).
    pub work_dir: Option<PathBuf>,
    /// `--mgmt-port` — override the default 8765 (None = default).
    pub mgmt_port: Option<u16>,
}

impl EngineConfig {
    /// A minimal portal-mode config; fill in the optionals as needed.
    pub fn portal(
        node_bin: PathBuf,
        agent_path: PathBuf,
        device_token: String,
        relay_addr: String,
        control_plane: String,
        mgmt_secret: String,
    ) -> Self {
        EngineConfig {
            node_bin,
            agent_path,
            mode: "portal".into(),
            device_token,
            relay_addr,
            control_plane,
            local_port: 8443,
            mgmt_secret,
            frp_token: None,
            frpc_bin: None,
            cert_mode: None,
            first_party_app: None,
            engine_version: None,
            work_dir: None,
            mgmt_port: None,
        }
    }

    /// Build the agent argument vector (everything after the program + agent.mjs).
    /// Optional flags are emitted only when set, so a bare config produces exactly
    /// the flags a plain portal agent needs.
    pub fn to_args(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "--mode".into(),
            self.mode.clone(),
            "--device-token".into(),
            self.device_token.clone(),
            "--relay-addr".into(),
            self.relay_addr.clone(),
            "--control-plane".into(),
            self.control_plane.clone(),
            "--local-port".into(),
            self.local_port.to_string(),
            "--mgmt-secret".into(),
            self.mgmt_secret.clone(),
        ];
        if let Some(ft) = self.frp_token.as_ref().filter(|s| !s.is_empty()) {
            a.push("--frp-token".into());
            a.push(ft.clone());
        }
        if let Some(fb) = &self.frpc_bin {
            a.push("--frpc-bin".into());
            a.push(fb.display().to_string());
        }
        if let Some(cm) = &self.cert_mode {
            a.push("--cert-mode".into());
            a.push(cm.clone());
        }
        if let Some(fp) = &self.first_party_app {
            a.push("--first-party-app".into());
            a.push(fp.clone());
        }
        if let Some(ev) = &self.engine_version {
            a.push("--engine-version".into());
            a.push(ev.clone());
        }
        if let Some(wd) = &self.work_dir {
            a.push("--work-dir".into());
            a.push(wd.display().to_string());
        }
        if let Some(mp) = self.mgmt_port {
            a.push("--mgmt-port".into());
            a.push(mp.to_string());
        }
        a
    }

    /// Build the spawn [`Command`] (program + agent.mjs + args). The caller may
    /// still set stdio, env, and platform creation flags before spawning.
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.node_bin);
        c.arg(&self.agent_path);
        c.args(self.to_args());
        c
    }

    /// Spawn the engine, inheriting null stdio unless the caller sets it first.
    pub fn spawn(&self) -> std::io::Result<Child> {
        let mut c = self.command();
        c.stdout(Stdio::null()).stderr(Stdio::null());
        c.spawn()
    }

    /// The management base URL this config's engine will listen on.
    pub fn mgmt_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.mgmt_port.unwrap_or(8765))
    }
}

/// The result of [`spawn_or_attach`].
pub enum StartOutcome {
    /// A compatible engine was already running; we registered against it.
    Attached(EngineInfo),
    /// No compatible engine — we spawned our own.
    Spawned(Child),
}

/// Discover a running engine and either **attach** to it (registering `app_id`)
/// or **spawn** a new one from `cfg`. This is the one call an embedding app makes
/// to guarantee exactly one engine is serving on this machine.
pub fn spawn_or_attach(
    cfg: &EngineConfig,
    app_id: &str,
    pid: u32,
) -> std::io::Result<StartOutcome> {
    let base = cfg.mgmt_base();
    if let Some(info) = discover(&base) {
        if decide_start_action(Some(&info), ENGINE_PROTOCOL) == StartAction::Attach {
            let _ = register(&base, app_id, pid);
            return Ok(StartOutcome::Attached(info));
        }
    }
    Ok(StartOutcome::Spawned(cfg.spawn()?))
}

/// Poll `GET /engine/info` until the engine answers or the deadline passes — a
/// freshly spawned engine needs a moment to bind its management port and learn
/// its identity before it can accept a publish.
pub fn wait_engine(mgmt_base: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if discover(mgmt_base).is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---------------------------------------------------------------------------
// Headless connect (Model A: user pays Meradomo, no Meradomo app download)
// ---------------------------------------------------------------------------

/// `POST /device/code` result: the one-time code and the URL the person approves
/// at in a browser.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCode {
    pub code: String,
    pub verify_url: String,
}

/// `GET /device/exchange` result.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    #[serde(default)]
    pub status: String, // "pending" | "approved" | "unknown"
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

/// Errors from the connect orchestration.
#[derive(Debug)]
pub enum ConnectError {
    Http(reqwest::Error),
    Io(std::io::Error),
    /// The device code expired or was never issued.
    CodeExpired,
    /// The approval window elapsed before the person finished in the browser.
    Timeout,
    /// The exchange succeeded but carried no device credential.
    NoCredential,
    /// The engine never became reachable after spawn.
    EngineUnreachable,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Http(e) => write!(f, "network error: {e}"),
            ConnectError::Io(e) => write!(f, "spawn error: {e}"),
            ConnectError::CodeExpired => write!(f, "the approval code expired — please try again"),
            ConnectError::Timeout => write!(f, "timed out waiting for approval in the browser"),
            ConnectError::NoCredential => write!(f, "approval returned no credential"),
            ConnectError::EngineUnreachable => write!(f, "the engine did not come up in time"),
        }
    }
}
impl std::error::Error for ConnectError {}
impl From<reqwest::Error> for ConnectError {
    fn from(e: reqwest::Error) -> Self {
        ConnectError::Http(e)
    }
}
impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        ConnectError::Io(e)
    }
}

/// Ask the control plane for a device code and the browser approval URL.
pub fn request_device_code(control_plane: &str) -> reqwest::Result<DeviceCode> {
    client()
        .post(format!("{control_plane}/device/code"))
        .timeout(Duration::from_secs(10))
        .send()?
        .json::<DeviceCode>()
}

/// Poll `GET /device/exchange` until the person finishes approving in the browser
/// (sign-in → name-claim → trial), or the window elapses.
pub fn poll_exchange(
    control_plane: &str,
    code: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<Exchange, ConnectError> {
    let deadline = Instant::now() + timeout;
    loop {
        let ex: Exchange = client()
            .get(format!("{control_plane}/device/exchange?code={code}"))
            .timeout(Duration::from_secs(10))
            .send()?
            .json()?;
        match ex.status.as_str() {
            "approved" => return Ok(ex),
            "unknown" => return Err(ConnectError::CodeExpired),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(ConnectError::Timeout);
        }
        std::thread::sleep(interval);
    }
}

/// What one app needs to go from a cold machine to a live public address.
pub struct ConnectRequest<'a> {
    /// Control-plane public URL (where the browser approves).
    pub control_plane: &'a str,
    /// This app's stable id (also the first-party id used for auto-approve).
    pub app_id: &'a str,
    /// The app label to publish (e.g. `"music"`).
    pub publish_name: &'a str,
    /// Human label shown for the published app.
    pub publish_label: &'a str,
    /// The app's local port to route to.
    pub local_port: u16,
    /// How long to wait for the person to finish approving in the browser.
    pub poll_timeout: Duration,
}

/// The result of a successful [`connect`].
pub struct Connected {
    pub device_token: String,
    pub host: String,
    pub publish: PublishResult,
    /// True if we attached to an engine already running; false if we spawned one.
    pub attached: bool,
}

/// The whole Model-A onboarding in one call: request a code, send the person to
/// the browser to sign in / claim their address / start the trial, wait for the
/// credential, persist it, start (or attach to) the engine, and publish this
/// app. Side-effects are injected so any host — and the tests — can drive it:
///
/// - `open_url(url)` opens the browser (a Tauri app uses its opener plugin).
/// - `persist(token, host)` stores the credential wherever the host keeps it.
/// - `build_config(token)` builds the [`EngineConfig`] once the token is known.
pub fn connect<O, P, B>(
    req: &ConnectRequest,
    pid: u32,
    open_url: O,
    persist: P,
    build_config: B,
) -> Result<Connected, ConnectError>
where
    O: FnOnce(&str),
    P: FnOnce(&str, &str),
    B: FnOnce(&str) -> EngineConfig,
{
    let dc = request_device_code(req.control_plane)?;
    open_url(&dc.verify_url);
    let ex = poll_exchange(req.control_plane, &dc.code, req.poll_timeout, Duration::from_secs(2))?;
    let token = ex.device_token.ok_or(ConnectError::NoCredential)?;
    let host = ex.host.unwrap_or_default();
    persist(&token, &host);

    let cfg = build_config(&token);
    let base = cfg.mgmt_base();
    let outcome = spawn_or_attach(&cfg, req.app_id, pid)?;
    let attached = matches!(outcome, StartOutcome::Attached(_));

    if !wait_engine(&base, Duration::from_secs(30)) {
        return Err(ConnectError::EngineUnreachable);
    }
    let publish = publish(&base, req.publish_name, req.publish_label, req.local_port, Some(req.app_id))?;
    Ok(Connected { device_token: token, host, publish, attached })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(protocol: u32) -> EngineInfo {
        EngineInfo { protocol, ..Default::default() }
    }

    #[test]
    fn attach_only_on_matching_protocol() {
        assert_eq!(decide_start_action(Some(&info(ENGINE_PROTOCOL)), ENGINE_PROTOCOL), StartAction::Attach);
        assert_eq!(decide_start_action(Some(&info(ENGINE_PROTOCOL + 1)), ENGINE_PROTOCOL), StartAction::Takeover);
        assert_eq!(decide_start_action(Some(&info(0)), ENGINE_PROTOCOL), StartAction::Takeover);
    }

    #[test]
    fn takeover_when_no_engine_answers() {
        assert_eq!(decide_start_action(None, ENGINE_PROTOCOL), StartAction::Takeover);
    }

    #[test]
    fn engine_info_parses_camelcase() {
        let j = r#"{"engineVersion":"0.4.0","protocol":1,"pid":42,"mode":"portal",
                    "connected":true,"host":"alice.meradomo.com","name":"alice",
                    "firstPartyApp":"com.example.app","registrants":2}"#;
        let i: EngineInfo = serde_json::from_str(j).unwrap();
        assert_eq!(i.engine_version, "0.4.0");
        assert_eq!(i.protocol, 1);
        assert_eq!(i.pid, 42);
        assert_eq!(i.connected, true);
        assert_eq!(i.name.as_deref(), Some("alice"));
        assert_eq!(i.first_party_app.as_deref(), Some("com.example.app"));
        assert_eq!(i.registrants, 2);
    }

    #[test]
    fn bare_config_emits_exactly_the_portal_flags() {
        let cfg = EngineConfig::portal(
            "node".into(),
            "agent.mjs".into(),
            "tok".into(),
            "relay:7000".into(),
            "http://cp:9002".into(),
            "secret".into(),
        );
        let args = cfg.to_args();
        assert_eq!(
            args,
            vec![
                "--mode", "portal",
                "--device-token", "tok",
                "--relay-addr", "relay:7000",
                "--control-plane", "http://cp:9002",
                "--local-port", "8443",
                "--mgmt-secret", "secret",
            ]
        );
    }

    #[test]
    fn optional_flags_appear_only_when_set() {
        let mut cfg = EngineConfig::portal(
            "node".into(), "a.mjs".into(), "t".into(), "r".into(), "c".into(), "s".into(),
        );
        cfg.frp_token = Some("ftok".into());
        cfg.frpc_bin = Some("/side/frpc".into());
        cfg.cert_mode = Some("acme".into());
        cfg.first_party_app = Some("com.example.app".into());
        cfg.engine_version = Some("0.4.0".into());
        let args = cfg.to_args();
        assert!(args.windows(2).any(|w| w == ["--frp-token", "ftok"]));
        assert!(args.windows(2).any(|w| w == ["--frpc-bin", "/side/frpc"]));
        assert!(args.windows(2).any(|w| w == ["--cert-mode", "acme"]));
        assert!(args.windows(2).any(|w| w == ["--first-party-app", "com.example.app"]));
        assert!(args.windows(2).any(|w| w == ["--engine-version", "0.4.0"]));
    }

    #[test]
    fn empty_frp_token_is_omitted() {
        let mut cfg = EngineConfig::portal(
            "node".into(), "a.mjs".into(), "t".into(), "r".into(), "c".into(), "s".into(),
        );
        cfg.frp_token = Some(String::new());
        assert!(!cfg.to_args().iter().any(|a| a == "--frp-token"));
    }

    #[test]
    fn mgmt_base_reflects_port() {
        let mut cfg = EngineConfig::portal(
            "node".into(), "a.mjs".into(), "t".into(), "r".into(), "c".into(), "s".into(),
        );
        assert_eq!(cfg.mgmt_base(), "http://127.0.0.1:8765");
        cfg.mgmt_port = Some(8790);
        assert_eq!(cfg.mgmt_base(), "http://127.0.0.1:8790");
    }

    #[test]
    fn device_code_parses() {
        let dc: DeviceCode = serde_json::from_str(
            r#"{"code":"abc123","verifyUrl":"https://account.meradomo.com/device/approve?code=abc123"}"#,
        )
        .unwrap();
        assert_eq!(dc.code, "abc123");
        assert!(dc.verify_url.contains("device/approve"));
    }

    #[test]
    fn exchange_pending_then_approved() {
        let pending: Exchange = serde_json::from_str(r#"{"status":"pending"}"#).unwrap();
        assert_eq!(pending.status, "pending");
        assert!(pending.device_token.is_none());

        let approved: Exchange = serde_json::from_str(
            r#"{"status":"approved","deviceToken":"tok-xyz","host":"alice.meradomo.com"}"#,
        )
        .unwrap();
        assert_eq!(approved.status, "approved");
        assert_eq!(approved.device_token.as_deref(), Some("tok-xyz"));
        assert_eq!(approved.host.as_deref(), Some("alice.meradomo.com"));
    }

    #[test]
    fn connect_error_messages_are_human() {
        assert!(ConnectError::Timeout.to_string().contains("browser"));
        assert!(ConnectError::CodeExpired.to_string().contains("expired"));
        assert!(ConnectError::EngineUnreachable.to_string().contains("engine"));
    }

    #[test]
    fn needs_renewal_only_on_lapse() {
        let mk = |s: &str| EngineInfo {
            billing: Some(BillingInfo { status: s.into(), ..Default::default() }),
            ..Default::default()
        };
        assert!(mk("hold").needs_renewal());
        assert!(mk("past_due").needs_renewal());
        assert!(!mk("active").needs_renewal());
        assert!(!mk("trialing").needs_renewal());
        assert!(!mk("comp").needs_renewal());
        // No billing info at all (e.g. attached to an engine that hasn't polled) → no prompt.
        assert!(!EngineInfo::default().needs_renewal());
    }

    #[test]
    fn engine_info_parses_billing() {
        let j = r#"{"protocol":1,"billing":{"entitled":false,"status":"hold","trialEndsAt":123}}"#;
        let i: EngineInfo = serde_json::from_str(j).unwrap();
        let b = i.billing.as_ref().unwrap();
        assert_eq!(b.entitled, false);
        assert_eq!(b.status, "hold");
        assert_eq!(b.trial_ends_at, Some(123));
        assert!(i.needs_renewal());
    }
}
