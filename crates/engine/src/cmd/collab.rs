//! Collaboration commands for agents and the UI (spike): status, invite
//! minting, the host's approvals and revocations.
//!
//! None of these touch the document, so they are all `pure()`: they never
//! enter undo and are never captured for peers.

use serde_json::{Value, json};

use crate::{CmdError, CmdResult, CommandSpec, Session, p};

/// The invite TTL cap: thirty days.
const MAX_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("collab.stats", "Collaboration Status", "Agents", |s, _| stats(s)).params("{}").pure(),
        CommandSpec::new("collab.invite", "Invite to Document", "Agents", invite)
            .params(r#"{"realm": string, "realmKey": string, "seed": string, "role": "editor"|"viewer", "ttlMs": number}"#)
            .pure(),
        CommandSpec::new("collab.approve", "Approve a Joiner", "Agents", approve).params(r#"{"node": string}"#).pure(),
        CommandSpec::new("collab.revoke", "Revoke a Holder", "Agents", revoke).params(r#"{"node": string}"#).pure(),
        CommandSpec::new("collab.pending", "Pending Join Requests", "Agents", |s, _| pending(s)).params("{}").pure(),
        CommandSpec::new("collab.realm", "Realm Membership", "Agents", |s, _| Ok(s.collab_realm())).params("{}").pure(),
    ]
}

fn stats(s: &mut Session) -> CmdResult {
    Ok(s.collab_stats())
}

fn role(v: &Value) -> Result<craft_collab::Role, CmdError> {
    match p::str(v, "role").unwrap_or("editor") {
        "editor" => Ok(craft_collab::Role::Editor),
        "viewer" => Ok(craft_collab::Role::Viewer),
        other => Err(CmdError::Params(format!("unknown role `{other}` (editor or viewer)"))),
    }
}

/// The host mints an invite line: the mesh coordinates to share (`realm`,
/// `seed`) plus a host-signed open token.
fn invite(s: &mut Session, v: &Value) -> CmdResult {
    let realm = p::req_str(v, "realm")?;
    let realm_key = p::str(v, "realmKey").unwrap_or("");
    let profile = p::str(v, "profile").unwrap_or("pq_hybrid");
    let seed = p::req_str(v, "seed")?;
    let role = role(v)?;
    let ttl = p::u64(v, "ttlMs").unwrap_or(24 * 60 * 60 * 1_000).min(MAX_TTL_MS);
    let line = s.collab_mint_invite(realm, realm_key, seed, profile, role, ttl).map_err(CmdError::Failed)?;
    Ok(json!({"invite": line}))
}

/// The host approves a pending joiner (the UI's People list does this too).
fn approve(s: &mut Session, v: &Value) -> CmdResult {
    let node = p::req_str(v, "node")?;
    s.collab_approve(node).map_err(CmdError::Failed)?;
    Ok(json!({"approved": node}))
}

/// The host withdraws a holder.
fn revoke(s: &mut Session, v: &Value) -> CmdResult {
    let node = p::req_str(v, "node")?;
    s.collab_revoke(node).map_err(CmdError::Failed)?;
    Ok(json!({"revoked": node}))
}

fn pending(s: &mut Session) -> CmdResult {
    Ok(json!({
        "pending": s.collab_pending().into_iter().map(|(node, name)| json!({"node": node, "name": name})).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn collab_commands_gate_on_being_the_host() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        // With no collaboration at all, the host-only commands refuse cleanly.
        assert!(s.run("collab.invite", &json!({"realm": "aa", "seed": "s"})).is_err());
        assert!(s.run("collab.approve", &json!({"node": "bb"})).is_err());
        assert!(s.run("collab.revoke", &json!({"node": "bb"})).is_err());
        assert_eq!(s.run("collab.pending", &json!({})).expect("pending")["pending"].as_array().map(Vec::len), Some(0));
        // A bad role is a params error, not a silent default at the door.
        assert!(s.run("collab.invite", &json!({"realm": "aa", "seed": "s", "role": "owner"})).is_err());
        assert!(s.run("collab.approve", &json!({})).is_err(), "node is required");
    }

    #[test]
    fn realm_status_reads_back_what_the_app_stored() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        assert_eq!(s.run("collab.realm", &json!({})).expect("realm")["signedIn"], false, "nothing stored yet");
        s.collab_set_realm(json!({"signedIn": true, "realm": "io.macula", "tier": "device", "ucanExpiresMs": 123}));
        let r = s.run("collab.realm", &json!({})).expect("realm");
        assert_eq!(r["tier"], "device");
        assert_eq!(r["ucanExpiresMs"], 123);
    }
}
