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
    /// The realm name for membership (sign-in) — `io.macula` by default.
    #[serde(default = "default_realm_name")]
    realm_name: String,
}

fn default_identity() -> String {
    "default".into()
}

fn default_profile() -> String {
    "pq_hybrid".into()
}

fn default_realm_name() -> String {
    "io.macula".into()
}

/// What the realm helpers need from this instance's identity, kept here
/// because the UI's sign-in hooks run outside the config's own scope.
struct RealmCtx {
    identity: Identity,
    realm_name: String,
    base: String,
    pending: Option<String>,
}

static REALM: std::sync::Mutex<Option<RealmCtx>> = std::sync::Mutex::new(None);

fn with_realm<T>(f: impl FnOnce(&mut RealmCtx) -> Result<T, String>) -> Result<T, String> {
    let mut guard = REALM.lock().map_err(|e| e.to_string())?;
    let ctx = guard.as_mut().ok_or_else(|| "this instance has no mesh identity yet".to_string())?;
    f(ctx)
}

/// Reads the stored membership into the session and returns it as the
/// `collab.realm` status object.
pub fn realm_refresh(session: &mut Session) -> Result<serde_json::Value, String> {
    let status = with_realm(|ctx| {
        let cred = macula_craft_collab::realm::load_credential(ctx.identity.node_id(), &ctx.realm_name);
        Ok(realm_status_json(&ctx.realm_name, cred.as_ref()))
    })?;
    session.collab_set_realm(status.clone());
    Ok(status)
}

fn realm_status_json(realm_name: &str, cred: Option<&macula_craft_collab::RealmCredential>) -> serde_json::Value {
    match cred {
        None => serde_json::json!({"signedIn": false, "realm": realm_name}),
        Some(cred) => serde_json::json!({
            "signedIn": true,
            "realm": cred.realm,
            "tier": match cred.tier {
                macula_craft_collab::Tier::Device => "device",
                macula_craft_collab::Tier::Citizen => "citizen",
            },
            "handle": cred.handle(),
            "orgIdentity": cred.org_identity,
            "joinedAtMs": cred.joined_at_ms,
            "ucanExpiresMs": cred.ucan_expires_ms(),
        }),
    }
}

/// Starts the realm's citizen join session: returns the link the person
/// opens (or scans) and remembers the session for [`sign_in_check`].
pub fn sign_in(session: &mut Session) -> Result<serde_json::Value, String> {
    let _ = session;
    let now = now_ms();
    with_realm(|ctx| {
        let created = macula_craft_collab::realm::session::create_session(&ctx.realm_name, &ctx.base, &ctx.identity, now)?;
        ctx.pending = Some(created.session_id.clone());
        Ok(serde_json::json!({
            "portal": ctx.base,
            "joinUrl": created.join_url,
            "sessionId": created.session_id,
            "expiresAt": created.expires_at,
        }))
    })
}

/// Polls the pending join session; on a confirmation stores the credential
/// (beside the identity keys) and returns the fresh membership status.
pub fn sign_in_check(session: &mut Session) -> Result<serde_json::Value, String> {
    let now = now_ms();
    let status = with_realm(|ctx| {
        let Some(session_id) = ctx.pending.clone() else {
            return Ok(serde_json::json!({"status": "none"}));
        };
        match macula_craft_collab::realm::session::poll_session(&ctx.base, &session_id)? {
            macula_craft_collab::realm::SessionStatus::Pending { expires_at } => Ok(serde_json::json!({"status": "pending", "expiresAt": expires_at})),
            macula_craft_collab::realm::SessionStatus::Expired => {
                ctx.pending = None;
                Ok(serde_json::json!({"status": "expired"}))
            }
            macula_craft_collab::realm::SessionStatus::Error(e) => Err(e),
            macula_craft_collab::realm::SessionStatus::Confirmed(confirmed) => {
                let cred = macula_craft_collab::realm::session::credential_from_confirmed(&ctx.identity, &ctx.realm_name, &ctx.base, confirmed, now);
                macula_craft_collab::realm::store_credential(&cred)?;
                ctx.pending = None;
                Ok(realm_status_json(&ctx.realm_name, Some(&cred)))
            }
        }
    })?;
    // Only a membership answer replaces the stored status: a pending poll
    // is about the session, not the membership.
    if status.get("signedIn").is_some() {
        session.collab_set_realm(status.clone());
    }
    Ok(status)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reads `path` and attaches `session` to the shared document over the mesh.
pub fn attach(session: &mut Session, path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let identity = Identity::load_or_create(&cfg.profile, &cfg.identity, &cfg.name)?;
    if let Ok(mut guard) = REALM.lock() {
        *guard = Some(RealmCtx {
            identity: identity.clone(),
            realm_name: cfg.realm_name.clone(),
            base: macula_craft_collab::realm::session::realm_base_url(&cfg.realm_name),
            pending: None,
        });
    }
    let _ = realm_refresh(session);
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
