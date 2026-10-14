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
    /// Share this document with a realm's members: any member presenting a
    /// live membership UCAN is admitted (with `share_role`).
    #[serde(default)]
    share_realm: Option<String>,
    /// The role realm members are admitted with: "viewer" (default) or
    /// "editor".
    #[serde(default)]
    share_role: Option<String>,
    /// Realms to add to this person's trust list before sharing: each a
    /// realm descriptor (`/.well-known/macula-realm.json`, saved to a file)
    /// and the fingerprint the person checked. A realm shared with that is
    /// not the transport's own must be trusted.
    #[serde(default)]
    trust_realms: Vec<TrustRealm>,
}

#[derive(Deserialize)]
struct TrustRealm {
    /// The realm the person means to trust: the descriptor must be for it.
    name: String,
    descriptor: String,
    fingerprint: String,
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

/// Installs the realm context wherever the identity is loaded: the startup
/// default, `--collab`, or an invite. The identity is per person/device,
/// not per document, so the Share window's realm row works from a plain
/// run.
fn set_realm_ctx(identity: &Identity, realm_name: &str) {
    set_realm_ctx_with_base(identity, realm_name, &macula_craft_collab::realm::session::realm_base_url(realm_name));
}

fn set_realm_ctx_with_base(identity: &Identity, realm_name: &str, base: &str) {
    if let Ok(mut guard) = REALM.lock() {
        *guard = Some(RealmCtx { identity: identity.clone(), realm_name: realm_name.to_string(), base: base.to_string(), pending: None });
    }
}

/// Whether a realm context is installed (the realm hooks can run).
#[cfg(test)]
pub fn realm_context_ready() -> bool {
    REALM.lock().map(|guard| guard.is_some()).unwrap_or(false)
}

/// The startup default: the person/device identity, loaded with an empty
/// name (a stored display name is never rewritten — a fresh identity is
/// named in the login window, never guessed here), its realm context
/// installed, so the Share window's realm row works before any document is
/// shared.
pub fn ensure_default_identity(session: &mut Session) -> Result<(), String> {
    let dir = macula_craft_collab::identity::craft_identity_dir().ok_or_else(|| "no per-user data directory for the identity".to_string())?;
    let identity = Identity::load_or_create("pq_hybrid", "default", "")?;
    let _ = dir;
    set_realm_ctx(&identity, &default_realm_name());
    // Seed the session's realm status too, so the windows and commands show
    // the realm name (and any stored membership) from the first frame.
    realm_refresh(session)?;
    Ok(())
}

/// [`ensure_default_identity`] at an explicit directory (tests and
/// embedders), file backend only.
#[cfg(test)]
pub fn ensure_default_identity_at(session: &mut Session, dir: &std::path::Path) -> Result<(), String> {
    let identity = Identity::load_or_create_at("pq_hybrid", "default", "", dir)?;
    set_realm_ctx(&identity, &default_realm_name());
    realm_refresh(session)?;
    Ok(())
}

/// The person's display name for this session's identity: what the login
/// window shows and edits. `default` is true while the name is still the
/// storage slug (a fresh identity nobody has named yet).
pub fn identity_name(session: &mut Session) -> Result<serde_json::Value, String> {
    let _ = session;
    with_realm(|ctx| {
        Ok(serde_json::json!({
            "name": ctx.identity.name(),
            "slug": ctx.identity.slug(),
            "default": ctx.identity.name_is_slug(),
        }))
    })
}

/// Sets the person's display name on this identity (persisted beside the
/// key; later startups keep it).
pub fn set_identity_name(session: &mut Session, name: &str) -> Result<serde_json::Value, String> {
    let _ = session;
    with_realm(|ctx| {
        ctx.identity.set_display_name(name)?;
        Ok(serde_json::json!({"name": ctx.identity.name(), "default": ctx.identity.name_is_slug()}))
    })
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
            macula_craft_collab::realm::SessionStatus::Pending { expires_at } => {
                Ok(serde_json::json!({"status": "pending", "expiresAt": expires_at}))
            }
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
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Reads `path` and attaches `session` to the shared document over the mesh.
pub fn attach(session: &mut Session, path: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let identity = Identity::load_or_create(&cfg.profile, &cfg.identity, &cfg.name)?;
    set_realm_ctx(&identity, &cfg.realm_name);
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
    let trust_path = macula_craft_collab::realm::trust::trust_path();
    if !cfg.trust_realms.is_empty() {
        let trust_path = trust_path.as_deref().ok_or("no config directory for the realm trust list")?;
        trust_realms(trust_path, &cfg.trust_realms).map_err(|e| format!("{path}: {e}"))?;
    }
    if let Some(realm) = cfg.share_realm.as_deref() {
        let role = match cfg.share_role.as_deref() {
            Some("editor") => craft_collab::Role::Editor,
            None | Some("viewer") => craft_collab::Role::Viewer,
            Some(other) => return Err(format!("{path}: unknown share_role `{other}` (viewer or editor)")),
        };
        let verifier = share_verifier(realm, role, &cfg.realm, &cfg.realm_key, trust_path.as_deref()).map_err(|e| format!("{path}: {e}"))?;
        session.collab_set_member_verifier(Box::new(verifier));
    }
    session.collab_set_share_context(&cfg.realm, &cfg.realm_key, &cfg.station, &cfg.profile);
    if let Some(path) = grants_path(&cfg.identity, &cfg.doc) {
        session.collab_set_grants_path(&path);
    }
    if let Some(path) = contacts_path(&cfg.identity) {
        session.collab_set_contacts_path(&path);
    }
    Ok(())
}

/// Joins through an invite line (the app's Share window, or a config): the
/// invite names the document, the host and its key, the realm and a seed
/// station, plus the host-signed open token this instance presents.
pub fn join_from_invite(session: &mut Session, invite_line: &str, name: &str) -> Result<(), String> {
    let identity = Identity::load_or_create("pq_hybrid", "default", name)?;
    // The invite carries the realm id, not its name; io.macula is today's
    // realm, and the realm row must work after pasting one too.
    set_realm_ctx(&identity, &default_realm_name());
    join_line(session, &identity, name, invite_line)
}

/// Adds each descriptor to the trust list at `path`, refusing the lot on the
/// first one that does not parse or whose fingerprint was not the one checked.
fn trust_realms(path: &std::path::Path, realms: &[TrustRealm]) -> Result<(), String> {
    let mut trust = macula_craft_collab::RealmTrust::load(path)?;
    for entry in realms {
        let text = std::fs::read_to_string(&entry.descriptor).map_err(|e| format!("{}: {e}", entry.descriptor))?;
        trust.trust_descriptor(&entry.name, &text, &entry.fingerprint)?;
    }
    trust.save(path)
}

/// The verifier for sharing with `realm`'s members. The transport's own
/// realm is already pinned by the config (its key is what advertisements
/// verify under); any other realm must be in this person's trust list.
fn share_verifier(
    realm: &str,
    role: craft_collab::Role,
    transport_realm_id: &str,
    transport_realm_key: &str,
    trust_path: Option<&std::path::Path>,
) -> Result<macula_craft_collab::RealmMemberVerifier, String> {
    let realm = realm.trim();
    let trust = match trust_path {
        Some(path) => macula_craft_collab::RealmTrust::load(path)?,
        None => macula_craft_collab::RealmTrust::default(),
    };
    let is_transport_realm = hex_bytes(transport_realm_id).is_ok_and(|id| id == macula_craft_collab::realm::realm_id_of(realm));
    if is_transport_realm && !transport_realm_key.is_empty() {
        let key = hex_bytes(transport_realm_key)?;
        // A checked trust entry for the same realm outranks an unchecked
        // config key: they must agree, or nothing is shared.
        if let Some(entry) = trust.entry(realm) {
            let pinned = macula_craft_collab::realm::trust::fingerprint(&key);
            if pinned != entry.key_id {
                return Err(format!("{realm}: the config's key ({pinned}) is not the one you trusted ({}); fix one of them", entry.key_id));
            }
        }
        return Ok(macula_craft_collab::RealmMemberVerifier::new(realm, key, role));
    }
    trust.verifier(realm, role)
}

fn join_line(session: &mut Session, identity: &Identity, name: &str, line: &str) -> Result<(), String> {
    let invite = craft_collab::Invite::decode(line.trim())?;
    // The joiner verifies who invited it before connecting anywhere.
    macula_craft_collab::verify_invite(&invite)?;
    // A realm invite carries no token: present this device's membership of
    // the realm the invite names (possibly not the host's own), when it
    // holds one.
    let membership = match (&invite.member_role, invite.member_realm.as_deref()) {
        (Some(_), Some(member_realm)) => macula_craft_collab::realm::load_credential(identity.node_id(), member_realm).and_then(|cred| cred.ucan),
        _ => None,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    if !invite.is_live(now) {
        return Err("this invite has expired".into());
    }
    let transport = MeshTransport::connect(&mesh_config(&invite.seed, &invite.realm, &invite.realm_key)?, identity)?;
    // Joining replaces the collaboration runtime, so the access table is
    // installed after it (installing first would be wiped).
    session.collab_join_requesting(&invite.doc, name, Box::new(transport), invite.token.clone(), membership);
    session.collab_set_access(macula_craft_collab::verified_access_from(&invite.doc, &invite.host_key, &invite.profile)?);
    // The host is the first person this joiner meets: remember them (the
    // invite carries their name) in this person's own contact list.
    if let Some(path) = contacts_path(identity.slug()) {
        session.collab_set_contacts_path(&path);
    }
    session.collab_note_contact(&invite.host, &invite.host_name, invite.host_realm.as_deref());
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

/// The per-person collaboration directory, when a config directory is
/// known: `craft/collab/`.
fn collab_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config")))?;
    let dir = base.join("craft").join("collab");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// A filename-safe slug for an identity or document id.
fn safe_slug(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Where the host's installed grants persist, when a config directory is
/// known: `craft/collab/<identity>.<doc>.grants.json`.
fn grants_path(identity: &str, doc: &str) -> Option<String> {
    let dir = collab_dir()?;
    Some(dir.join(format!("{}.{}.grants.json", safe_slug(identity), safe_slug(doc))).to_string_lossy().to_string())
}

/// Where this person's contacts persist, when a config directory is known:
/// `craft/collab/<identity>.contacts.json` — the person's, not the
/// document's, so every session shares one list.
fn contacts_path(identity: &str) -> Option<String> {
    let dir = collab_dir()?;
    Some(dir.join(format!("{}.contacts.json", safe_slug(identity))).to_string_lossy().to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    const IO_MACULA: &str = include_str!("../tests/vectors/io.macula.well-known.json");
    const IO_MACULA_ID: &str = "abb81b5a614b63551b400b810648c0c8a78efad845442630c94b46cc95d2fcd1";
    const IO_MACULA_FP: &str = "a50fef96e52b4d68ad112ff42b55dc916cd70e5b283280e8cf3cc5e492459cf6";

    /// Cross-realm at the app: sharing with a realm that is not the
    /// transport's needs it in the trust list, entered with the checked
    /// fingerprint; the transport's own realm is already pinned by config.
    #[test]
    fn sharing_with_another_realm_needs_it_trusted_under_the_checked_fingerprint() {
        use craft_collab::MembershipVerifier as _;
        let dir = tempfile::tempdir().expect("dir");
        let trust = dir.path().join("trusted-realms.json");
        let descriptor = dir.path().join("io.macula.json");
        std::fs::write(&descriptor, IO_MACULA).expect("descriptor");
        let other_transport = "cc".repeat(32);

        // Not the transport's realm and not trusted: refused, with the fix named.
        let Err(err) = share_verifier("io.macula", craft_collab::Role::Editor, &other_transport, "ee", Some(&trust)) else { panic!("refused") };
        assert!(err.contains("trusted realms"), "{err}");

        // A fingerprint that is not the key's: nothing is trusted.
        let wrong = [TrustRealm { name: "io.macula".into(), descriptor: descriptor.display().to_string(), fingerprint: "00".repeat(32) }];
        assert!(trust_realms(&trust, &wrong).is_err());
        assert!(share_verifier("io.macula", craft_collab::Role::Editor, &other_transport, "ee", Some(&trust)).is_err());

        // The checked fingerprint: trusted, and the verifier checks that realm.
        // A descriptor offered under another name: refused (Fable round 1).
        let renamed = [TrustRealm { name: "net.beam-campus".into(), descriptor: descriptor.display().to_string(), fingerprint: IO_MACULA_FP.into() }];
        assert!(trust_realms(&trust, &renamed).is_err(), "the person names the realm, not the descriptor");
        assert!(share_verifier("net.beam-campus", craft_collab::Role::Editor, &other_transport, "ee", Some(&trust)).is_err());

        let right = [TrustRealm { name: "io.macula".into(), descriptor: descriptor.display().to_string(), fingerprint: IO_MACULA_FP.into() }];
        trust_realms(&trust, &right).expect("trusted");
        let verifier = share_verifier("io.macula", craft_collab::Role::Editor, &other_transport, "ee", Some(&trust)).expect("now trusted");
        assert_eq!(verifier.realm(), "io.macula");
        assert_eq!(verifier.role(), craft_collab::Role::Editor);
    }

    /// Fable round 1, required 2: an unchecked config key never overrides a
    /// checked trust entry for the same realm.
    #[test]
    fn a_config_key_that_disagrees_with_the_trusted_one_shares_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let trust = dir.path().join("trusted-realms.json");
        let descriptor = dir.path().join("io.macula.json");
        std::fs::write(&descriptor, IO_MACULA).expect("descriptor");
        trust_realms(
            &trust,
            &[TrustRealm { name: "io.macula".into(), descriptor: descriptor.display().to_string(), fingerprint: IO_MACULA_FP.into() }],
        )
        .expect("trusted");
        let Err(err) = share_verifier("io.macula", craft_collab::Role::Viewer, IO_MACULA_ID, "ee", Some(&trust)) else {
            panic!("a config key that is not the trusted one")
        };
        assert!(err.contains(IO_MACULA_FP), "{err}");
        let (_, key) = macula_craft_collab::RealmDescriptor::parse(IO_MACULA).expect("vector");
        let key_hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        share_verifier("io.macula", craft_collab::Role::Viewer, IO_MACULA_ID, &key_hex, Some(&trust)).expect("agreeing keys share");
    }

    #[test]
    fn the_transport_realm_is_shared_with_under_its_pinned_key() {
        use craft_collab::MembershipVerifier as _;
        let dir = tempfile::tempdir().expect("dir");
        let verifier = share_verifier("io.macula", craft_collab::Role::Viewer, IO_MACULA_ID, "ee", Some(&dir.path().join("none.json")))
            .expect("pinned by config");
        assert_eq!(verifier.realm(), "io.macula");
    }

    /// Raf's live repro, fixed and green: a plain run (no `--collab`)
    /// pressing Share then Sign in. The startup default loads the identity,
    /// so the realm hooks reach the membership (and, for sign-in, the
    /// portal) instead of refusing with an identity error.
    #[test]
    fn a_plain_run_reaches_the_realm_hooks_without_the_identity_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::new(wordcraft_doc::Document::new());
        ensure_default_identity_at(&mut session, dir.path()).expect("the startup default loads an identity");
        assert!(realm_context_ready(), "the realm context is installed by the startup default");

        let status = realm_refresh(&mut session).expect("realm_refresh works from a plain run");
        assert_eq!(status["signedIn"], false, "no membership yet, and no error");

        // sign_in's own guard passes too: against an unreachable portal the
        // failure is the transport's, never a missing identity.
        let identity = Identity::load_or_create_at("pq_hybrid", "default", "", dir.path()).expect("identity");
        set_realm_ctx_with_base(&identity, "io.macula", "http://127.0.0.1:9");
        let err = sign_in(&mut session).expect_err("an unreachable portal fails the call");
        assert!(!err.contains("identity"), "not the plain-run identity error: {err}");
    }

    /// The login window is where a person names a fresh identity, and the
    /// startup path never guesses or clobbers: it loads with an empty name,
    /// and the chosen name survives every later startup.
    #[test]
    fn a_stored_name_is_never_clobbered_by_the_startup_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut session = Session::new(wordcraft_doc::Document::new());
        ensure_default_identity_at(&mut session, dir.path()).expect("the startup default loads an identity");
        let before = identity_name(&mut session).expect("the identity has a name");
        assert_eq!(before["default"], true, "a fresh identity is unnamed (its name is the slug)");
        set_identity_name(&mut session, "First Name").expect("the login window names it");
        assert_eq!(identity_name(&mut session).expect("name")["name"], "First Name");
        assert!(set_identity_name(&mut session, "   ").is_err(), "an empty name is refused");

        let mut session = Session::new(wordcraft_doc::Document::new());
        ensure_default_identity_at(&mut session, dir.path()).expect("a later startup keeps the stored name");
        let again = identity_name(&mut session).expect("name");
        assert_eq!(again["name"], "First Name", "the stored display name survives");
        assert_eq!(again["default"], false, "and is no longer the slug");
    }
}
