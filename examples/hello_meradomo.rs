//! Hello Meradomo — the canonical worked example for embedding the Meradomo
//! engine (Model A: the user pays Meradomo, no Meradomo app download).
//!
//! A real host is a Tauri app whose one "Share with Meradomo" button calls
//! [`meradomo_engine::connect`]; this example is that same call with the
//! side-effects wired to a console + the real system browser, so it can be run
//! and read end to end:
//!
//! ```sh
//! # From a bundled engine dir produced by scripts/build-engine-bundle.sh:
//! ENGINE_BUNDLE=./dist/engine-bundle \
//! MERADOMO_CONTROL_PLANE=https://account.meradomo.com \
//! LOCAL_PORT=3000 \
//!   cargo run --example hello_meradomo
//! ```
//!
//! It walks the person through account + address + trial in the browser, then
//! serves whatever is running on `LOCAL_PORT` at `https://music.<name>.meradomo.com`.
//!
//! The bundle layout it expects (see build-engine-bundle.sh):
//!   $ENGINE_BUNDLE/sidecars/node-<triple>
//!   $ENGINE_BUNDLE/sidecars/frpc-<triple>
//!   $ENGINE_BUNDLE/resources/agent/agent.mjs
//!   $ENGINE_BUNDLE/resources/agent/bundle.json   (engineVersion/protocol)

use std::path::PathBuf;
use std::time::Duration;

use meradomo_engine::{connect, ConnectRequest, EngineConfig};

/// This reference app's stable id — also its first-party id, so its own publish
/// auto-approves while any other app attaching to the shared engine would pend.
const APP_ID: &str = "com.example.hello-meradomo";

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Pick the sidecar for this build target. A real Tauri app lets Tauri strip the
/// triple; here we resolve it ourselves so the example is self-contained.
fn sidecar(bundle: &str, name: &str) -> PathBuf {
    let triple = if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    };
    let specific = PathBuf::from(bundle).join("sidecars").join(format!("{name}-{triple}"));
    if specific.exists() {
        specific
    } else {
        PathBuf::from(bundle).join("sidecars").join(format!("{name}-universal-apple-darwin"))
    }
}

fn main() {
    let bundle = env("ENGINE_BUNDLE", "./dist/engine-bundle");
    let control_plane = env("MERADOMO_CONTROL_PLANE", "https://account.meradomo.com");
    let local_port: u16 = env("LOCAL_PORT", "3000").parse().unwrap_or(3000);

    let node_bin = sidecar(&bundle, "node");
    let frpc_bin = sidecar(&bundle, "frpc");
    let agent_path = PathBuf::from(&bundle).join("resources").join("agent").join("agent.mjs");

    let req = ConnectRequest {
        control_plane: &control_plane,
        app_id: APP_ID,
        publish_name: "music",
        publish_label: "Music",
        local_port,
        poll_timeout: Duration::from_secs(5 * 60),
    };

    // A real Tauri app persists the credential in its app-data dir and reloads it
    // on next launch to skip the browser step. Here we just print it.
    let persisted: std::cell::RefCell<Option<(String, String)>> = std::cell::RefCell::new(None);

    let result = connect(
        &req,
        std::process::id(),
        // open_url — send the person to the browser to sign in / claim / pay.
        |url| {
            println!("Opening your browser to finish setting up:\n  {url}");
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(url).status();
        },
        // persist — store the credential wherever the host keeps it.
        |token, host| {
            println!("Connected as {host} (credential {} chars)", token.len());
            *persisted.borrow_mut() = Some((token.to_string(), host.to_string()));
        },
        // build_config — build the spawn config once the credential is known.
        |token| EngineConfig {
            node_bin: node_bin.clone(),
            agent_path: agent_path.clone(),
            mode: "portal".into(),
            device_token: token.to_string(),
            relay_addr: env("MERADOMO_RELAY_ADDR", "connect.meradomo.com:7000"),
            control_plane: control_plane.clone(),
            local_port: 8443,
            mgmt_secret: String::new(), // a real app generates a random secret
            frp_token: std::env::var("MERADOMO_FRP_TOKEN").ok(),
            frpc_bin: Some(frpc_bin.clone()),
            cert_mode: Some("acme".into()),
            first_party_app: Some(APP_ID.to_string()),
            engine_version: None, // read from bundle.json beside agent.mjs
            work_dir: None,
            mgmt_port: None,
        },
    );

    match result {
        Ok(c) => {
            println!("Live at {}", c.publish.url.as_deref().unwrap_or("(pending)"));
            println!("attached-to-existing-engine: {}", c.attached);
        }
        Err(e) => eprintln!("Could not connect: {e}"),
    }
}
