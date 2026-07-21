# meradomo-engine

The Rust SDK for embedding the **[Meradomo](https://meradomo.com) engine** in your
app — so your users get a public web address for their local app with **one
download, not two**.

This crate is the open-source **launcher/client**: it discovers a running engine,
spawns one from a bundled payload, attaches to a shared engine, and publishes your
app — all over the engine's loopback HTTP protocol. The engine binary itself is a
separate, pre-built download (see below).

```toml
[dependencies]
meradomo-engine = "0.1"
```

## Quick start

```rust
use meradomo_engine::{connect, ConnectRequest, EngineConfig};
use std::time::Duration;

let req = ConnectRequest {
    control_plane: "https://account.meradomo.com",
    app_id: "com.example.myapp",   // your app id (also its first-party id)
    publish_name: "music",         // → music.<name>.meradomo.com
    publish_label: "Music",
    local_port: 3000,              // where your app is listening
    poll_timeout: Duration::from_secs(5 * 60),
};

let connected = connect(
    &req,
    std::process::id(),
    |url| open_in_browser(url),                 // send the user to sign in / pay
    |token, host| save_credential(token, host), // persist for next launch
    |token| EngineConfig { /* bundled node/frpc/agent.mjs paths … */ ..cfg(token) },
)?;

println!("Live at {}", connected.publish.url.unwrap_or_default());
```

On the next launch, reuse the saved credential and call
[`spawn_or_attach`](https://docs.rs/meradomo-engine) — no browser. See the worked
example in [`examples/hello_meradomo.rs`](examples/hello_meradomo.rs).

## The engine bundle (closed, pre-built)

The engine binary (`agent.mjs` + a pinned Node runtime + the outbound client) is
the Meradomo product and ships as a **versioned, checksummed download**, indexed by
a public manifest:

```
https://stmeraqqemciyamqs2e.blob.core.windows.net/releases/engine-sdk/manifest.json
```

Fetch the manifest, download `bundle.url`, verify `bundle.sha256`, and drop the
`sidecars/` + `resources/` into your app's bundle (Tauri `externalBin` +
`resources`). The manifest also carries the wire `protocol` version — attach to an
incumbent engine only when its protocol matches [`ENGINE_PROTOCOL`].

## The three integration paths

1. **Detect the Meradomo app** (no bundle) — the plain [Agent API](https://developers.meradomo.com/guides/agent-api).
2. **Embed the engine, Model A** — the user pays Meradomo; **one download**. This crate.
3. **Embed the engine, Model B** — you pay Meradomo for usage; your users need no Meradomo account. *Coming soon.*

Full docs: **https://developers.meradomo.com/guides/embedded-engine**

## License

MIT.
