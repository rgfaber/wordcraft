//! `--collab <config.json>`: attach this instance to a document shared over the mesh.
//!
//! The config (spike format):
//! ```json
//! {
//!   "station": "<host>:<port>:<station node id hex>",
//!   "realm": "<realm id hex>",
//!   "realm_key": "<realm key hex>",
//!   "doc": "demo",
//!   "name": "Alice",
//!   "join": false,
//!   "profile": "pq_hybrid"
//! }
//! ```
//! `join` makes this instance ask a peer for the document snapshot first (a
//! joining editor); without it, the instance shares its own document.

use serde::Deserialize;
use wordcraft_engine::Session;

#[derive(Deserialize)]
struct Config {
    station: String,
    realm: String,
    realm_key: String,
    doc: String,
    name: String,
    #[serde(default)]
    join: bool,
    #[serde(default = "default_profile")]
    profile: String,
}

fn default_profile() -> String {
    "pq_hybrid".into()
}

/// Reads `path` and attaches `session` to the shared document over the mesh.
pub fn attach(session: &mut Session, path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let mut parts = cfg.station.rsplitn(3, ':');
    let (Some(node_hex), Some(port), Some(host)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!("{path}: station must be host:port:node_hex"));
    };
    let mesh = macula_craft_collab::MeshConfig {
        host: host.to_string(),
        port: port.parse::<u16>().map_err(|e| format!("{path}: {e}"))?,
        station_node_id: hex32(node_hex).map_err(|e| format!("{path}: station node id: {e}"))?,
        realm_id: hex32(&cfg.realm).map_err(|e| format!("{path}: realm: {e}"))?,
        realm_key: hex_bytes(&cfg.realm_key).map_err(|e| format!("{path}: realm key: {e}"))?,
        profile: cfg.profile,
    };
    let transport = macula_craft_collab::MeshTransport::connect(&mesh)?;
    if cfg.join {
        session.collab_join(&cfg.doc, &cfg.name, Box::new(transport));
    } else {
        session.collab_attach(&cfg.doc, &cfg.name, Box::new(transport));
    }
    Ok(())
}

fn hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("an odd number of hex digits".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string())).collect()
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    let v = hex_bytes(s)?;
    let n = v.len();
    v.try_into().map_err(|_| format!("expected 32 bytes, got {n}"))
}
