//! `--collab <config.json>`: attach this instance to a document shared over the mesh.
//!
//! The config (spike format), host side:
//! ```json
//! {
//!   "station": "<host>:<port>:<station node id hex>",
//!   "realm": "<realm id hex>",
//!   "realm_key": "<realm key hex>",
//!   "doc": "demo",
//!   "name": "Alice",
//!   "identity": "alice",
//!   "join": false,
//!   "auto_approve": true,
//!   "profile": "pq_hybrid"
//! }
//! ```
//! and joiner side, with an invite line from the host instead of the raw
//! coordinates:
//! ```json
//! { "name": "Bob", "identity": "bob", "invite": "{...}" }
//! ```
//! `identity` is the storage slug of this person's persistent Craft key (one
//! key per person per device, shared by every Craft app; secure store, file
//! fallback headless). `join` makes this instance ask a peer for the document
//! snapshot first; without it, the instance shares its own document. A host
//! instance answers join requests with grants (`auto_approve` skips the
//! manual yes; the app's Share window approves by hand).

use serde::Deserialize;
use wordcraft_engine::Session;

use macula_craft_collab::{Identity, IdentityIssuer, MeshConfig, MeshTransport};

#[derive(Deserialize)]
struct Config {
    /// `<host>:<port>:<station node id hex>`; also what an invite carries.
    #[serde(default)]
    station: String,
    #[serde(default)]
    realm: String,
    #[serde(default)]
    realm_key: String,
    #[serde(default)]
    doc: String,
    name: String,
    #[serde(default = "default_identity")]
    identity: String,
    #[serde(default)]
    join: bool,
    /// The host admits a valid invite token without asking by hand.
    #[serde(default)]
    auto_approve: bool,
    /// An invite line: join through the invite flow instead of the raw
    /// station/realm fields.
    #[serde(default)]
    invite: Option<String>,
    #[serde(default = "default_profile")]
    profile: String,
}

fn default_identity() -> String {
    "default".into()
}

fn default_profile() -> String {
    "pq_hybrid".into()
}

/// Reads `path` and attaches `session` to the shared document over the mesh.
pub fn attach(session: &mut Session, path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let identity = Identity::load_or_create(&cfg.profile, &cfg.identity, &cfg.name)?;
    if let Some(line) = &cfg.invite {
        join_line(session, &identity, &cfg.name, line)?;
        return Ok(());
    }
    if cfg.station.is_empty() || cfg.realm.is_empty() {
        return Err(format!("{path}: station and realm are required without an invite"));
    }
    let transport = MeshTransport::connect(&mesh_config(&cfg.station, &cfg.realm, &cfg.realm_key)?, &identity)?;
    // A raw-coordinates config is the document owner's instance: it holds
    // the access table, mints grants and can invite. `join` only says whether
    // this instance asks the peers for the document state (a host restart
    // reopens as a join; a fresh owner does not need to).
    if cfg.join {
        session.collab_join(&cfg.doc, &cfg.name, Box::new(transport));
    } else {
        session.collab_attach(&cfg.doc, &cfg.name, Box::new(transport));
    }
    session.collab_set_access(identity.verified_access(&cfg.doc));
    session.collab_set_issuer(Box::new(IdentityIssuer::new(identity.clone())));
    session.collab_set_auto_approve(cfg.auto_approve);
    session.collab_set_share_context(&cfg.realm, &cfg.realm_key, &cfg.station, &cfg.profile);
    if let Some(path) = grants_path(&cfg.identity, &cfg.doc) {
        session.collab_set_grants_path(&path);
    }
    Ok(())
}

/// Joins through an invite line (the app's Share window, or a config): the
/// invite names the document, the host and its key, the realm and a seed
/// station, plus the host-signed open token this instance presents.
pub fn join_from_invite(session: &mut Session, invite_line: &str, name: &str) -> Result<(), String> {
    let identity = Identity::load_or_create("pq_hybrid", "default", name)?;
    join_line(session, &identity, name, invite_line)
}

fn join_line(session: &mut Session, identity: &Identity, name: &str, line: &str) -> Result<(), String> {
    let invite = craft_collab::Invite::decode(line.trim())?;
    // The joiner verifies who invited it before connecting anywhere.
    macula_craft_collab::verify_invite(&invite)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if !invite.is_live(now) {
        return Err("this invite has expired".into());
    }
    let transport = MeshTransport::connect(&mesh_config(&invite.seed, &invite.realm, &invite.realm_key)?, identity)?;
    // Joining replaces the collaboration runtime, so the access table is
    // installed after it (installing first would be wiped).
    session.collab_join_via_invite(&invite.doc, name, Box::new(transport), invite.token);
    session.collab_set_access(macula_craft_collab::verified_access_from(&invite.doc, &invite.host_key, &invite.profile)?);
    Ok(())
}

fn mesh_config(seed: &str, realm: &str, realm_key: &str) -> Result<MeshConfig, String> {
    let mut parts = seed.rsplitn(3, ':');
    let (Some(node_hex), Some(port), Some(host)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("a station seed must be host:port:node_hex".into());
    };
    Ok(MeshConfig {
        host: host.to_string(),
        port: port.parse::<u16>().map_err(|e| format!("station port: {e}"))?,
        station_node_id: hex32(node_hex).map_err(|e| format!("station node id: {e}"))?,
        realm_id: hex32(realm).map_err(|e| format!("realm: {e}"))?,
        realm_key: hex_bytes(realm_key).map_err(|e| format!("realm key: {e}"))?,
    })
}

/// Where the host's installed grants persist, when a config directory is
/// known: `craft/collab/<identity>.<doc>.grants.json`.
fn grants_path(identity: &str, doc: &str) -> Option<String> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config")))?;
    let dir = base.join("craft").join("collab");
    std::fs::create_dir_all(&dir).ok()?;
    let safe = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
    Some(dir.join(format!("{}.{}.grants.json", safe(identity), safe(doc))).to_string_lossy().to_string())
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
