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
            .params(r#"{"realm": string, "realmKey": string, "seed": string, "role": "editor"|"viewer", "ttlMs": number, "forNode"?: hex, "forName"?: string, "realmMembers"?: 1}"#)
            .pure(),
        CommandSpec::new("collab.approve", "Approve a Joiner", "Agents", approve).params(r#"{"node": string}"#).pure(),
        CommandSpec::new("collab.revoke", "Revoke a Holder", "Agents", revoke).params(r#"{"node": string}"#).pure(),
        CommandSpec::new("collab.pending", "Pending Join Requests", "Agents", |s, _| pending(s)).params("{}").pure(),
        CommandSpec::new("collab.contacts", "List Contacts", "Agents", |s, _| contacts(s)).params("{}").pure(),
        CommandSpec::new("collab.forget_contact", "Forget a Contact", "Agents", forget_contact).params(r#"{"node": string}"#).pure(),
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
    let realm_members = p::bool(v, "realmMembers").unwrap_or(false);
    let for_node = p::str(v, "forNode");
    let for_name = p::str(v, "forName").unwrap_or("");
    let line = if let Some(node) = for_node {
        // An invite for one person: the token names their node id, and only
        // that node is admitted at the door.
        s.collab_mint_invite_for(realm, realm_key, seed, profile, node, for_name, role, ttl)
    } else if realm_members {
        // No open token: members of the host's realm are admitted with the
        // verifier's role (the host must share with a realm).
        s.collab_mint_realm_invite(realm, realm_key, seed, profile, ttl)
    } else {
        s.collab_mint_invite(realm, realm_key, seed, profile, role, ttl)
    }
    .map_err(CmdError::Failed)?;
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

fn contacts(s: &mut Session) -> CmdResult {
    Ok(json!({
        "contacts": s.collab_contacts().into_iter().map(|c| json!({
            "node": c.node,
            "name": c.name,
            "lastSeenMs": c.last_seen_ms,
            "realm": c.realm,
        })).collect::<Vec<_>>(),
    }))
}

/// Forgets a remembered person (the Share window's Forget button does this too).
fn forget_contact(s: &mut Session, v: &Value) -> CmdResult {
    let node = p::req_str(v, "node")?;
    Ok(json!({"forgotten": s.collab_forget_contact(node)}))
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

    #[test]
    fn contacts_are_listed_and_forgotten_by_command() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        assert_eq!(s.run("collab.contacts", &json!({})).expect("contacts")["contacts"].as_array().map(Vec::len), Some(0));
        s.collab_attach("demo", "Me", Box::new(craft_collab::MemoryTransport::bus(&["aaaaaaaa"]).remove(0)));
        s.collab_note_contact("bbbbbbbb", "Bob", Some("io.macula"));
        let listed = s.run("collab.contacts", &json!({})).expect("contacts");
        assert_eq!(listed["contacts"][0]["name"], "Bob");
        assert_eq!(listed["contacts"][0]["realm"], "io.macula");
        assert_eq!(s.run("collab.forget_contact", &json!({"node": "bbbbbbbb"})).expect("forget")["forgotten"], true);
        assert_eq!(s.run("collab.contacts", &json!({})).expect("contacts")["contacts"].as_array().map(Vec::len), Some(0));
        assert!(s.run("collab.forget_contact", &json!({})).is_err(), "node is required");
    }
}
