//! The collaboration glue: craft-collab's shared mechanism attached to a session.
//!
//! Wordcraft's adoption of the seam. Local typing is captured as an exact
//! paragraph text update (stable pid, caret, text); every other mutating
//! command is captured as a [`Command`](craft_collab::Kind::Command) op with
//! the selection it ran under, which the peers replay through the same
//! registry with [`Origin::Remote`](craft_collab::Origin). Peer messages are
//! applied in per-author sequence (text updates are order-insensitive; a
//! command is not), outside the local undo history; undo reverts only this
//! author's own edits. Received edits also carry a latency sample.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use wordcraft_doc::{Document, Pos};

use crate::{CmdError, Session};

/// How long a grant issued on a join lasts, when the caller names nothing.
const GRANT_TTL_MS: u64 = 24 * 60 * 60 * 1_000;

/// How many times a joiner asks for admission before giving up.
const MAX_GRANT_ATTEMPTS: u32 = 5;

/// Everything a shared session needs: the peers' transport, the text CRDT,
/// the ordering buffer, the presence table and the latency samples.
pub struct CollabRuntime {
    doc: String,
    name: String,
    author: String,
    transport: Box<dyn craft_collab::Transport>,
    texts: craft_collab::TextMerge,
    presence: craft_collab::Presence,
    order: craft_collab::OrderBuffer,
    /// Received-edit latency per author, milliseconds.
    latency: HashMap<String, Vec<u64>>,
    /// Remote commands that failed to apply (a divergence signal).
    remote_errors: u64,
    /// Messages dropped by the access check (stranger, expired, wrong role).
    unauthorized: u64,
    /// Who may act on this document, when access control is on.
    access: Option<craft_collab::Access>,
    seq: u64,
    /// This process's epoch: a restart keeps its author id but sends a new
    /// epoch, so peers never confuse its restarted sequence with the old.
    epoch: u64,
    last_cursor: Option<String>,
    /// This session asked for the document snapshot and is waiting for it.
    awaiting_snapshot: bool,
    /// How many snapshot requests have been sent, and when the last was.
    snapshot_attempts: u32,
    last_snapshot_ms: u64,
    /// The host's signing side, when this session is the host: what mints
    /// grants and revocations on a join.
    issuer: Option<Box<dyn craft_collab::GrantIssuer>>,
    /// A joiner's open token: presented until a grant arrives.
    token: Option<craft_collab::SignedGrant>,
    /// A joiner's realm membership UCAN, when it joins by membership.
    membership: Option<String>,
    /// The host's realm-member verifier: when set, the document is shared
    /// with that realm and a live membership admits.
    member_verifier: Option<Box<dyn craft_collab::MembershipVerifier>>,
    /// This joiner is waiting for its grant.
    awaiting_grant: bool,
    grant_attempts: u32,
    last_grant_ms: u64,
    /// Join requests waiting for the host's yes: node id -> (name, at_ms,
    /// role, and the membership's own end when the admission was one).
    pending: std::collections::BTreeMap<String, (String, u64, craft_collab::Role, Option<u64>)>,
    /// Whether the host admits a valid token without asking.
    auto_approve: bool,
    /// Whether this session holds the document: the origin (attach) does
    /// from the start; a joiner only once it has loaded a snapshot. An
    /// approved joiner that has not caught up yet must never answer another
    /// joiner's snapshot request with its empty document.
    has_snapshot: bool,
    /// The mesh coordinates this host shares in: (realm, realm key, seed,
    /// signing profile). Opaque here; the invite carries them for a joiner.
    share: Option<(String, String, String, String)>,
    /// Where the installed grants persist, when the app names a file: a
    /// restarted host re-learns its holders from it.
    grants_path: Option<std::path::PathBuf>,
    /// The people this device has met (join requests, approved grants, the
    /// host named by an invite), loaded from `contacts_path`.
    contacts: craft_collab::Contacts,
    /// Where the contacts persist, when the app names a file: they belong
    /// to the person, not the document, so one file serves every session.
    contacts_path: Option<std::path::PathBuf>,
}

impl CollabRuntime {
    fn new(doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) -> CollabRuntime {
        let author = transport.author();
        let texts = craft_collab::TextMerge::new(&author);
        CollabRuntime {
            doc: doc.to_string(),
            name: name.to_string(),
            author,
            transport,
            texts,
            presence: craft_collab::Presence::default(),
            order: craft_collab::OrderBuffer::default(),
            latency: HashMap::new(),
            remote_errors: 0,
            unauthorized: 0,
            access: None,
            seq: 0,
            epoch: now_ms().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (std::process::id() as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9),
            last_cursor: None,
            awaiting_snapshot: false,
            snapshot_attempts: 0,
            last_snapshot_ms: 0,
            issuer: None,
            token: None,
            membership: None,
            member_verifier: None,
            awaiting_grant: false,
            grant_attempts: 0,
            last_grant_ms: 0,
            pending: std::collections::BTreeMap::new(),
            auto_approve: false,
            has_snapshot: false,
            share: None,
            grants_path: None,
            contacts: craft_collab::Contacts::new(),
            contacts_path: None,
        }
    }

    /// The messages for this document from other authors, in per-author
    /// sequence.
    fn poll(&mut self) -> Vec<craft_collab::Message> {
        let doc = self.doc.clone();
        let author = self.author.clone();
        let mut out = Vec::new();
        for msg in self.transport.poll() {
            if msg.doc == doc && msg.author != author {
                out.extend(self.order.push(msg));
            }
        }
        out
    }

    /// Queues a message for the peers.
    fn send(&mut self, kind: craft_collab::Kind) {
        self.seq += 1;
        let msg = craft_collab::Message {
            doc: self.doc.clone(),
            author: self.author.clone(),
            epoch: self.epoch,
            seq: self.seq,
            sent_at_ms: now_ms(),
            kind,
        };
        let _ = self.transport.send(msg);
    }
}

/// Milliseconds since the epoch (0 where there is no clock).
/// Milliseconds since the UNIX epoch, 0 on wasm: a clockless peer holds no
/// live grant (grants require a wall clock to be live), so collaboration is
/// only wired where a real clock exists (native builds; wasm has no
/// transport).
pub(crate) fn now_ms() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
}

/// A peer to draw: their name, colour and cursor position in this document.
pub struct PeerView {
    pub name: String,
    pub color: [u8; 3],
    pub pos: Option<Pos>,
}

impl Session {
    /// Starts collaborating on `doc` as `name`, over `transport`.
    pub fn collab_attach(&mut self, doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) {
        let mut runtime = CollabRuntime::new(doc, name, transport);
        // The origin starts with the document itself.
        runtime.has_snapshot = true;
        self.collab = Some(runtime);
    }

    /// Joins an already-shared `doc` as `name`: the first tick asks a peer
    /// for the document snapshot, so stable paragraph ids line up before
    /// this author types.
    pub fn collab_join(&mut self, doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) {
        let mut runtime = CollabRuntime::new(doc, name, transport);
        runtime.awaiting_snapshot = true;
        self.collab = Some(runtime);
    }

    /// Whether this session is sharing its document.
    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    /// Turns on access control for this document: only the host and live
    /// grant holders act. Without it the session is open (spike mode).
    pub fn collab_set_access(&mut self, access: craft_collab::Access) {
        if let Some(c) = self.collab.as_mut() {
            c.access = Some(access);
        }
    }

    /// Installs the host's signing side: with it, this session answers join
    /// requests with grants.
    pub fn collab_set_issuer(&mut self, issuer: Box<dyn craft_collab::GrantIssuer>) {
        if let Some(c) = self.collab.as_mut() {
            c.issuer = Some(issuer);
        }
    }

    /// Whether the host admits a valid token without asking (the UI leaves
    /// this off and approves from `collab_pending`).
    pub fn collab_set_auto_approve(&mut self, on: bool) {
        if let Some(c) = self.collab.as_mut() {
            c.auto_approve = on;
        }
    }

    /// Remembers the mesh coordinates this host shares in, so the UI can
    /// mint an invite with one call.
    pub fn collab_set_share_context(&mut self, realm: &str, realm_key: &str, seed: &str, profile: &str) {
        if let Some(c) = self.collab.as_mut() {
            c.share = Some((realm.to_string(), realm_key.to_string(), seed.to_string(), profile.to_string()));
        }
    }

    /// The holders of this document (the UI's People list), sorted.
    pub fn collab_holders(&self) -> Vec<String> {
        self.collab.as_ref().and_then(|c| c.access.as_ref()).map(|a| a.holder_ids()).unwrap_or_default()
    }

    /// Names the file where installed grants persist (the app's choice),
    /// and loads it: a restarted host re-learns its holders and a restarted
    /// holder its own grant. Malformed or unverifiable entries are skipped;
    /// persistence never takes the collaboration down.
    pub fn collab_set_grants_path(&mut self, path: &str) {
        let now = now_ms();
        if let Some(c) = self.collab.as_mut() {
            c.grants_path = Some(std::path::PathBuf::from(path));
        }
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let Ok(grants) = serde_json::from_str::<Vec<craft_collab::SignedGrant>>(&text) else { return };
        if let Some(c) = self.collab.as_mut()
            && let Some(access) = c.access.as_mut()
        {
            for signed in &grants {
                let _ = access.install(signed, now);
            }
        }
    }

    /// Writes the installed grants to the persisted file, if one is named.
    fn collab_save_grants(&self) {
        let Some(c) = self.collab.as_ref() else { return };
        let Some(path) = c.grants_path.as_ref() else { return };
        let Some(access) = c.access.as_ref() else { return };
        let grants = access.signed_grants();
        if let Ok(json) = serde_json::to_string(&grants) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Names the file where this person's contacts persist (the app's
    /// choice), and loads it: a restarted session keeps knowing the people
    /// it met. Malformed entries are skipped; persistence never takes the
    /// collaboration down.
    pub fn collab_set_contacts_path(&mut self, path: &str) {
        if let Ok(text) = std::fs::read_to_string(path) {
            let contacts = craft_collab::Contacts::from_json(&text);
            if let Some(c) = self.collab.as_mut() {
                c.contacts = contacts;
            }
        }
        if let Some(c) = self.collab.as_mut() {
            c.contacts_path = Some(std::path::PathBuf::from(path));
        }
    }

    /// Remembers one sighting of a person: their node id, the name they gave
    /// (empty keeps what we have) and the realm, when known.
    pub fn collab_note_contact(&mut self, node: &str, name: &str, realm: Option<&str>) {
        let now = now_ms();
        if let Some(c) = self.collab.as_mut() {
            c.contacts.upsert(node, name, realm, now);
        }
        self.collab_save_contacts();
    }

    /// The people this device has met, most recently seen first.
    pub fn collab_contacts(&self) -> Vec<craft_collab::Contact> {
        self.collab.as_ref().map(|c| c.contacts.list()).unwrap_or_default()
    }

    /// Forgets a person; true when one was known.
    pub fn collab_forget_contact(&mut self, node: &str) -> bool {
        let forgotten = self.collab.as_mut().is_some_and(|c| c.contacts.remove(node));
        if forgotten {
            self.collab_save_contacts();
        }
        forgotten
    }

    /// Writes the contacts to the persisted file, if one is named.
    fn collab_save_contacts(&self) {
        let Some(c) = self.collab.as_ref() else { return };
        let Some(path) = c.contacts_path.as_ref() else { return };
        let _ = std::fs::write(path, c.contacts.to_json());
    }

    /// Stores the realm membership status the app read for this session's
    /// identity (`collab.realm` reports it back).
    pub fn collab_set_realm(&mut self, status: serde_json::Value) {
        self.realm = Some(status);
    }

    /// The realm membership status, or a bare not-signed-in answer.
    pub fn collab_realm(&self) -> serde_json::Value {
        self.realm.clone().unwrap_or_else(|| serde_json::json!({"signedIn": false}))
    }

    /// Whether this session can mint invites (it is the host and knows its
    /// mesh coordinates).
    pub fn collab_can_invite(&self) -> bool {
        self.collab.as_ref().is_some_and(|c| c.issuer.is_some() && c.share.is_some())
    }

    /// Mints an invite for one person from the remembered share context:
    /// the token names their node id, and only that node is admitted.
    pub fn collab_invite_for_from_context(
        &mut self,
        holder: &str,
        invitee_name: &str,
        role: craft_collab::Role,
        ttl_ms: u64,
    ) -> Result<String, String> {
        let (realm, realm_key, seed, profile) = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some((realm, realm_key, seed, profile)) = c.share.as_ref() else {
                return Err("this session does not know its mesh coordinates".into());
            };
            (realm.clone(), realm_key.clone(), seed.clone(), profile.clone())
        };
        self.collab_mint_invite_for(&realm, &realm_key, &seed, &profile, holder, invitee_name, role, ttl_ms)
    }

    /// Mints an invite from the remembered share context (the UI's Copy
    /// invite): `role` and `ttl_ms` as in `collab_mint_invite`.
    pub fn collab_invite_from_context(&mut self, role: craft_collab::Role, ttl_ms: u64) -> Result<String, String> {
        let (realm, realm_key, seed, profile) = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some((realm, realm_key, seed, profile)) = c.share.as_ref() else {
                return Err("this session does not know its mesh coordinates".into());
            };
            (realm.clone(), realm_key.clone(), seed.clone(), profile.clone())
        };
        self.collab_mint_invite(&realm, &realm_key, &seed, &profile, role, ttl_ms)
    }

    /// Joins `doc` through an invite: presents its open token until the host
    /// grants this node, then asks for the snapshot.
    pub fn collab_join_via_invite(&mut self, doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>, token: craft_collab::SignedGrant) {
        self.collab_join_requesting(doc, name, transport, Some(token), None);
    }

    /// Joins `doc` presenting either admission: the invite's open token, a
    /// realm membership UCAN, or both (a realm invite carries no token and
    /// admits by membership).
    pub fn collab_join_requesting(
        &mut self,
        doc: &str,
        name: &str,
        transport: Box<dyn craft_collab::Transport>,
        token: Option<craft_collab::SignedGrant>,
        membership: Option<String>,
    ) {
        let mut runtime = CollabRuntime::new(doc, name, transport);
        runtime.token = token;
        runtime.membership = membership;
        runtime.awaiting_grant = true;
        self.collab = Some(runtime);
    }

    /// The host shares this document with a realm: members presenting a live
    /// membership UCAN of that realm are admitted with the verifier's role,
    /// and the grant issued on one is capped at the membership's own end.
    pub fn collab_set_member_verifier(&mut self, verifier: Box<dyn craft_collab::MembershipVerifier>) {
        if let Some(c) = self.collab.as_mut() {
            c.member_verifier = Some(verifier);
        }
    }

    /// Whether this host shares with a realm.
    pub fn collab_shares_with_realm(&self) -> bool {
        matches!(&self.collab, Some(c) if c.member_verifier.is_some())
    }

    /// Join requests awaiting the host's yes: (node id, display name).
    pub fn collab_pending(&self) -> Vec<(String, String)> {
        self.collab.as_ref().map(|c| c.pending.iter().map(|(node, (name, _, _, _))| (node.clone(), name.clone())).collect()).unwrap_or_default()
    }

    /// The host approves a pending joiner: mints the grant from its token's
    /// role (capped by the membership's end when the admission was one),
    /// broadcasts it and installs it.
    pub fn collab_approve(&mut self, node: &str) -> Result<(), String> {
        let (role, ends_ms) = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some((_, _, role, ends_ms)) = c.pending.get(node) else {
                return Err("no such pending joiner".into());
            };
            (*role, *ends_ms)
        };
        if self.collab.as_ref().and_then(|c| c.issuer.as_ref()).is_none() {
            return Err("this session is not the host".into());
        }
        self.collab_issue_grant(node, role, now_ms(), ends_ms);
        Ok(())
    }

    /// The host revokes a holder: signs it, broadcasts it, applies it.
    pub fn collab_revoke(&mut self, node: &str) -> Result<(), String> {
        let now = now_ms();
        let signed = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some(issuer) = c.issuer.as_ref() else { return Err("this session is not the host".into()) };
            issuer.revoke(&c.doc, node, now)?
        };
        if let Some(c) = self.collab.as_mut() {
            c.send(craft_collab::Kind::Revoked { revocation: signed.clone() });
            if let Some(access) = c.access.as_mut() {
                let _ = access.revoke(&signed, now);
            }
            c.pending.remove(node);
        }
        Ok(())
    }

    /// Mints this host's invite for the document: `realm`, `realm_key` and
    /// `seed` are the mesh coordinates to share, verbatim. Returns the
    /// copyable line.
    pub fn collab_mint_invite(
        &mut self,
        realm: &str,
        realm_key: &str,
        seed: &str,
        profile: &str,
        role: craft_collab::Role,
        ttl_ms: u64,
    ) -> Result<String, String> {
        self.collab_mint_invite_kind(realm, realm_key, seed, profile, role, ttl_ms, false, None)
    }

    /// Mints a realm invite: no open token, and members of the host's realm
    /// are admitted with the verifier's role. The host must share with a
    /// realm (`collab_set_member_verifier`).
    pub fn collab_mint_realm_invite(&mut self, realm: &str, realm_key: &str, seed: &str, profile: &str, ttl_ms: u64) -> Result<String, String> {
        let role = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some(verifier) = c.member_verifier.as_ref() else {
                return Err("this document is not shared with a realm".into());
            };
            verifier.role()
        };
        self.collab_mint_invite_kind(realm, realm_key, seed, profile, role, ttl_ms, true, None)
    }

    /// Mints an invite for one person: the token's holder is that person's
    /// node id, so only that node is admitted at the door. The caller knows
    /// the node id (from contacts or the person's own message).
    #[allow(clippy::too_many_arguments)]
    pub fn collab_mint_invite_for(
        &mut self,
        realm: &str,
        realm_key: &str,
        seed: &str,
        profile: &str,
        holder: &str,
        invitee_name: &str,
        role: craft_collab::Role,
        ttl_ms: u64,
    ) -> Result<String, String> {
        let holder = holder.trim().to_ascii_lowercase();
        if holder.len() != 64 || !holder.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("an invitee node id is 64 hex characters".into());
        }
        self.collab_mint_invite_kind(realm, realm_key, seed, profile, role, ttl_ms, false, Some((&holder, invitee_name)))
    }

    #[allow(clippy::too_many_arguments)]
    fn collab_mint_invite_kind(
        &mut self,
        realm: &str,
        realm_key: &str,
        seed: &str,
        profile: &str,
        role: craft_collab::Role,
        ttl_ms: u64,
        realm_members: bool,
        holder: Option<(&str, &str)>,
    ) -> Result<String, String> {
        let now = now_ms();
        // The host's own realm, when it is signed in to one: what the joiner
        // shows beside its name.
        let host_realm = self.realm.as_ref().filter(|r| r["signedIn"] == true).and_then(|r| r["realm"].as_str()).map(str::to_string);
        let (doc, host, host_key, host_name, token, member_realm) = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some(issuer) = c.issuer.as_ref() else { return Err("this session is not the host".into()) };
            // A realm invite names the realm whose members it admits: the
            // verifier's, which may be another realm than the host's own.
            let member_realm = match (realm_members, c.member_verifier.as_ref()) {
                (true, Some(verifier)) => Some(verifier.realm().to_string()),
                (true, None) => return Err("this document is not shared with a realm".into()),
                (false, _) => None,
            };
            let token = match holder {
                // An invite for one person: the token is pre-named for them.
                Some((node, _name)) => issuer.issue(&c.doc, node, role, ttl_ms, now)?,
                None => issuer.open_token(&c.doc, role, ttl_ms, now)?,
            };
            (c.doc.clone(), issuer.node_id(), issuer.carried_key(), c.name.clone(), token, member_realm)
        };
        let mut invite = craft_collab::Invite {
            v: craft_collab::INVITE_VERSION,
            doc,
            host,
            host_key: hex_bytes(&host_key),
            host_name,
            realm: realm.to_string(),
            realm_key: realm_key.to_string(),
            profile: profile.to_string(),
            seed: seed.to_string(),
            issued_ms: now,
            expires_ms: now.saturating_add(ttl_ms),
            token: if realm_members { None } else { Some(token) },
            member_role: if realm_members { Some(role) } else { None },
            member_realm,
            host_realm,
            invitee: holder.map(|(node, _)| node.to_string()),
            invitee_name: holder.map(|(_, name)| name.to_string()),
            signature: Vec::new(),
        };
        // The envelope is signed by the host identity: the joiner can verify
        // exactly who invited it, and that no field was swapped en route.
        invite.signature = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some(issuer) = c.issuer.as_ref() else { return Err("this session is not the host".into()) };
            issuer.sign(&invite.signed_bytes()?)?
        };
        invite.encode()
    }

    /// The exact edit a local command is about to make, when it is known
    /// (typing): paragraph, byte index, replaced length, text, and the
    /// formatting that text will get (so peers' runs match). `None` means
    /// the command goes as a replayable [`Command`](craft_collab::Kind::Command)
    /// instead.
    pub(crate) fn collab_exact_plan(&self, id: &str, params: &Value) -> Option<(u64, u32, u32, String, Option<Vec<u8>>)> {
        if id != "text.insert" {
            return None;
        }
        let text = params.get("text").and_then(Value::as_str)?;
        if text.contains(['\r', '\u{2029}']) {
            return None;
        }
        let (a, b) = self.sel.ordered();
        if a.story != b.story || a.path != b.path {
            return None;
        }
        let pid = self.doc.para_at(&a)?.pid;
        if pid == 0 {
            return None;
        }
        let props = serde_json::to_vec(&self.typing_props()).ok();
        Some((pid, a.off as u32, (b.off - a.off) as u32, text.to_string(), props))
    }

    /// Applies a captured exact edit to the CRDT and shares it, healing any
    /// difference between the CRDT and the document (autocorrect and the
    /// like) in the same step. `props` is the formatting the inserted text
    /// got, carried to the peers.
    pub(crate) fn collab_apply_exact(&mut self, pid: u64, index: u32, remove: u32, insert: String, props: Option<Vec<u8>>) {
        let para = pid.to_string();
        let Some(doc_text) = self.doc.paragraph_by_pid(pid).and_then(|(s, p)| self.doc.para(s, &p).map(|p| p.text.clone())) else {
            return;
        };
        let mut updates = Vec::new();
        if let Some(c) = self.collab.as_mut() {
            if let Some(u) = c.texts.local_edit_exact(&para, index, remove, &insert) {
                updates.push((u, props.clone()));
            }
            if let Some(u) = c.texts.sync(&para, &doc_text) {
                updates.push((u, None));
            }
        }
        for (update, props) in updates {
            if let Some(c) = self.collab.as_mut() {
                c.send(craft_collab::Kind::TextEdit { para: para.clone(), update, props });
            }
        }
    }

    /// The command to replay on the peers for a local mutating command the
    /// exact path did not take: its id, its params as JSON, and the
    /// selection it runs under (both ends as pid + offset), so a
    /// position-carrying command (a style change) is applied as it was
    /// issued. `None` for commands that must not be shared (see
    /// [`replayable_command`]).
    pub(crate) fn collab_command_plan(&self, id: &str, params: &Value) -> Option<(String, Vec<u8>, Option<String>)> {
        if !replayable_command(id) {
            return None;
        }
        let bytes = serde_json::to_vec(params).ok()?;
        Some((id.to_string(), bytes, self.selection_json()))
    }

    /// The anchored style this command applies: only `format.bold` for now,
    /// applied through the CRDT so it covers exactly its author's text.
    pub(crate) fn collab_style_plan(&self, id: &str, params: &Value) -> Option<(u64, u32, u32, bool)> {
        if id != "format.bold" {
            return None;
        }
        let (a, b) = self.sel.ordered();
        if a == b || a.story != b.story || a.path != b.path {
            return None;
        }
        let para = self.doc.para_at(&a)?;
        let (pid, len) = (para.pid, b.off.saturating_sub(a.off));
        if pid == 0 || len == 0 {
            return None;
        }
        let bold = match params.get("value").and_then(Value::as_bool) {
            Some(v) => v,
            None => (a.off..b.off).any(|i| para.props_of_char(i).bold != Some(true)),
        };
        Some((pid, a.off as u32, len as u32, bold))
    }

    /// Applies a captured bold to the CRDT, shares it, and aligns the local
    /// runs to the CRDT's spans.
    pub(crate) fn collab_apply_style(&mut self, pid: u64, index: u32, len: u32, bold: bool) {
        let para = pid.to_string();
        let update = match self.collab.as_mut() {
            Some(c) => c.texts.local_bold(&para, index, len, bold),
            None => return,
        };
        if let Some(update) = update
            && let Some(c) = self.collab.as_mut()
        {
            c.send(craft_collab::Kind::TextEdit { para: para.clone(), update, props: None });
        }
        self.collab_sync_bold(pid);
    }

    /// Aligns a paragraph's bold to the CRDT's item-anchored spans: the
    /// derivation every replica shares, so runs cover the same characters.
    pub(crate) fn collab_sync_bold(&mut self, pid: u64) {
        let spans = match self.collab.as_ref() {
            Some(c) => c.texts.bold_spans(&pid.to_string()),
            None => return,
        };
        if self.doc.set_paragraph_bold_by_pid(pid, &spans) {
            self.touch();
        }
    }

    /// Queues a captured command for the peers.
    pub(crate) fn collab_send_command(&mut self, id: String, params: Vec<u8>, anchor: Option<String>) {
        if let Some(c) = self.collab.as_mut() {
            c.send(craft_collab::Kind::Command { id, params, anchor });
        }
    }

    /// The selection as the app encodes it: both ends as pid + offset.
    fn selection_json(&self) -> Option<String> {
        let encode = |p: &Pos| -> Option<Value> {
            let para = self.doc.para_at(p)?;
            (para.pid != 0).then(|| json!({"pid": para.pid, "off": p.off.min(para.len())}))
        };
        Some(json!({"anchor": encode(&self.sel.anchor)?, "focus": encode(&self.sel.focus)?}).to_string())
    }

    /// Applies what the peers sent and shares this author's cursor. The app
    /// calls this once per frame while collaborating.
    pub fn collab_tick(&mut self) {
        let msgs = match self.collab.as_mut() {
            Some(c) => c.poll(),
            None => return,
        };
        for msg in msgs {
            self.collab_receive(msg);
        }
        // A joiner asks for admission until its grant arrives.
        let now = now_ms();
        if let Some(c) = self.collab.as_mut()
            && c.awaiting_grant
            && c.grant_attempts < MAX_GRANT_ATTEMPTS
            && (c.grant_attempts == 0 || now.saturating_sub(c.last_grant_ms) >= 2_000)
            && (c.token.is_some() || c.membership.is_some())
        {
            c.grant_attempts += 1;
            c.last_grant_ms = now;
            let (token, membership) = (c.token.clone(), c.membership.clone());
            c.send(craft_collab::Kind::JoinRequest { token, name: c.name.clone(), membership });
        }

        // A joiner asks for the shared document until it hears one back (a
        // publication can be lost; the request is cheap and idempotent).
        let now = now_ms();
        if let Some(c) = self.collab.as_mut()
            && c.awaiting_snapshot
            && c.snapshot_attempts < 5
            && (c.snapshot_attempts == 0 || now.saturating_sub(c.last_snapshot_ms) >= 2_000)
        {
            c.snapshot_attempts += 1;
            c.last_snapshot_ms = now;
            c.send(craft_collab::Kind::SnapshotRequest {});
        }
        self.collab_send_cursor();
    }

    fn collab_receive(&mut self, msg: craft_collab::Message) {
        // A join request is the door: the host judges it by its token, not
        // by a holder lookup (the joiner holds no grant yet).
        if let craft_collab::Kind::JoinRequest { token, name, membership } = &msg.kind {
            self.collab_on_join_request(&msg.author, token, membership, name);
            return;
        }
        // Access control, on every message: the host and live grant holders
        // act; strangers, expired grants and role violations are dropped.
        if let Some(admission) = self.collab.as_ref().and_then(|c| c.access.as_ref()).map(|a| a.admits(&msg.author, &msg.kind, now_ms()))
            && !admission.is_admitted()
        {
            if let Some(c) = self.collab.as_mut() {
                c.unauthorized += 1;
            }
            return;
        }
        match msg.kind {
            craft_collab::Kind::TextEdit { para, update, props } => {
                self.collab_record_latency(&msg.author, msg.sent_at_ms);
                let merged = match self.collab.as_mut() {
                    Some(c) => c.texts.apply_update(&para, &update),
                    None => return,
                };
                let Ok(merged) = merged else { return };
                let Ok(pid) = para.parse::<u64>() else { return };
                let carried: Option<wordcraft_doc::CharProps> = props.as_deref().and_then(|b| serde_json::from_slice(b).ok());
                if let Some((story, path, pre, removed, inserted)) = self.doc.splice_paragraph_text(pid, &merged, carried.as_ref()) {
                    // The local caret and selection follow the remote change
                    // made before them, so the next local typing lands where
                    // its author is, not inside the other person's text.
                    for pos in [&mut self.sel.anchor, &mut self.sel.focus] {
                        if pos.story == story && pos.path == path {
                            pos.off = shifted_off(pos.off, pre, removed, inserted);
                        }
                    }
                    self.touch();
                    self.clamp_selection();
                }
                // The CRDT also carries the anchored formatting: derive
                // runs from it, so every replica covers the same characters.
                self.collab_sync_bold(pid);
            }
            craft_collab::Kind::Command { id, params, anchor } => {
                self.collab_record_latency(&msg.author, msg.sent_at_ms);
                // A holder may replay only the commands the local capture
                // would ever send; anything else (file, tools, UI, undo) is
                // refused on receive, whoever the author is.
                if !replayable_command(&id) {
                    if let Some(c) = self.collab.as_mut() {
                        c.unauthorized += 1;
                    }
                    return;
                }
                // Replay under the command's own selection, but leave the
                // local author's caret exactly as it was: the replay must
                // not turn the next local typing into a replacement.
                let previous = self.sel.clone();
                if let Some(sel) = anchor.as_deref().and_then(|s| selection_from(&self.doc, s)) {
                    self.sel = sel;
                }
                let parsed: Value = serde_json::from_slice(&params).unwrap_or(Value::Null);
                let result = self.run_with_origin(&id, &parsed, craft_collab::Origin::Remote { author: msg.author.clone() });
                self.sel = previous;
                self.clamp_selection();
                if let Err(e) = result {
                    if let Some(c) = self.collab.as_mut() {
                        c.remote_errors += 1;
                    }
                    self.status = format!("remote {id}: {e}");
                }
            }
            craft_collab::Kind::CursorMoved { position, name } => {
                let name = name.unwrap_or_else(|| msg.author.chars().take(6).collect());
                if let Some(c) = self.collab.as_mut() {
                    c.presence.set(&msg.author, &name, position);
                }
            }
            craft_collab::Kind::SnapshotRequest {} => {
                // Only a session that holds the document answers: a joiner
                // still waiting for its own catch-up has nothing to give
                // (and its empty document would win the race against the
                // host's real snapshot).
                if matches!(&self.collab, Some(c) if !c.has_snapshot) {
                    return;
                }
                let data = craft_collab::DocAdapter::snapshot(self);
                let crdt = self.collab.as_ref().map(|c| c.texts.full_update()).unwrap_or_default();
                if let Some(c) = self.collab.as_mut() {
                    c.send(craft_collab::Kind::Snapshot { data, crdt });
                }
            }
            craft_collab::Kind::Snapshot { data, crdt } => {
                let joining = matches!(&self.collab, Some(c) if c.awaiting_snapshot);
                if joining && craft_collab::DocAdapter::load_snapshot(self, &data).is_ok() {
                    // Seed the CRDT too, or post-join merges would not align
                    // with the peers' item history.
                    if let Some(c) = self.collab.as_mut() {
                        let _ = c.texts.apply_doc_update(&crdt);
                        c.awaiting_snapshot = false;
                        c.has_snapshot = true;
                    }
                }
            }
            craft_collab::Kind::GrantIssued { grant } => {
                // Every replica installs it (the verifier decides); a joiner
                // this grant names is admitted and may now ask for the doc.
                let now = now_ms();
                if let Some(c) = self.collab.as_mut() {
                    if let Some(access) = c.access.as_mut() {
                        let _ = access.install(&grant, now);
                    }
                    if c.awaiting_grant && grant.grant.holder == c.author {
                        c.awaiting_grant = false;
                        c.awaiting_snapshot = true;
                        c.snapshot_attempts = 0;
                    }
                }
                self.collab_save_grants();
            }
            craft_collab::Kind::Revoked { revocation } => {
                let now = now_ms();
                if let Some(c) = self.collab.as_mut()
                    && let Some(access) = c.access.as_mut()
                {
                    let _ = access.revoke(&revocation, now);
                }
                self.collab_save_grants();
            }
            // Handled before the access check: it is the door itself.
            craft_collab::Kind::JoinRequest { .. } => {}
        }
    }

    /// The host judges a join request by its token: a valid open token gets
    /// a concrete grant (auto-approved when configured, else queued for the
    /// host's yes); an invalid one is refused and counted.
    fn collab_on_join_request(&mut self, author: &str, token: &Option<craft_collab::SignedGrant>, membership: &Option<String>, name: &str) {
        let now = now_ms();
        let mut admitted: Option<(craft_collab::Role, Option<u64>)> = None;
        {
            let Some(c) = self.collab.as_mut() else { return };
            if c.issuer.is_none() {
                return; // not the host: only the host admits
            }
            // The invite's own token, when one was presented.
            if let Some(token) = token
                && let Some(access) = c.access.as_ref()
                && let Ok(checked) = access.check_token_for(token, author, now)
            {
                admitted = Some((checked.role, None));
            }
            // A realm membership: the verifier holds the realm's key and the
            // host's role mapping; the audience check pins the caller, and
            // the membership's own end caps any grant issued on it.
            if admitted.is_none()
                && let (Some(verifier), Some(ucan)) = (c.member_verifier.as_ref(), membership.as_ref())
                && let Ok(ends_ms) = verifier.verify_member(ucan, author, now)
            {
                admitted = Some((verifier.role(), Some(ends_ms)));
            }
        }
        let Some((role, ends_ms)) = admitted else {
            // A stranger at the door: refused and counted (the old code
            // returned silently; the count is the honest signal).
            if let Some(c) = self.collab.as_mut()
                && c.issuer.is_some()
            {
                c.unauthorized += 1;
            }
            return;
        };
        // A membership that has already ended is not an admission: there is
        // nothing to grant, so the door refuses like any other stranger.
        if ends_ms.is_some_and(|ends| ends <= now) {
            if let Some(c) = self.collab.as_mut()
                && c.issuer.is_some()
            {
                c.unauthorized += 1;
            }
            return;
        }
        // The person at the door is someone we have met: remember them. A
        // stranger with a bad token never reaches this point.
        self.collab_note_contact(author, name, None);
        let auto = matches!(&self.collab, Some(c) if c.auto_approve);
        if auto {
            self.collab_issue_grant(author, role, now, ends_ms);
        } else if let Some(c) = self.collab.as_mut() {
            c.pending.insert(author.to_string(), (name.to_string(), now, role, ends_ms));
        }
    }

    /// Mints a grant for `holder` as the host, broadcasts it and installs it.
    /// `ends_ms` is the membership's own end when the admission was one: the
    /// grant never outlives it, so a revoked or lapsed membership takes the
    /// document access down with it.
    fn collab_issue_grant(&mut self, holder: &str, role: craft_collab::Role, now: u64, ends_ms: Option<u64>) {
        let ttl_ms = match ends_ms {
            Some(ends) => GRANT_TTL_MS.min(ends.saturating_sub(now)),
            None => GRANT_TTL_MS,
        };
        if ttl_ms == 0 {
            return; // nothing left of the membership: nothing to grant
        }
        let issued = {
            let Some(c) = self.collab.as_ref() else { return };
            let Some(issuer) = c.issuer.as_ref() else { return };
            match issuer.issue(&c.doc, holder, role, ttl_ms, now) {
                Ok(signed) => signed,
                Err(_) => return,
            }
        };
        if let Some(c) = self.collab.as_mut() {
            c.send(craft_collab::Kind::GrantIssued { grant: issued.clone() });
            if let Some(access) = c.access.as_mut() {
                let _ = access.install(&issued, now);
            }
            c.pending.remove(holder);
        }
        self.collab_save_grants();
    }

    /// Records one received-edit latency sample (skipped when the sender had
    /// no clock).
    fn collab_record_latency(&mut self, author: &str, sent_at_ms: u64) {
        if sent_at_ms == 0 {
            return;
        }
        let now = now_ms();
        if let Some(c) = self.collab.as_mut() {
            c.latency.entry(author.to_string()).or_default().push(now.saturating_sub(sent_at_ms));
        }
    }

    /// The collaboration status for agents: peers, join state, per-pair
    /// latency (p50/p95) and the count of remote commands that failed to
    /// apply (a divergence signal).
    pub fn collab_stats(&self) -> Value {
        let Some(c) = self.collab.as_ref() else {
            return json!({"active": false});
        };
        let peers: Vec<Value> = c
            .presence
            .peers()
            .map(|p| {
                json!({
                    "name": p.name,
                    "author": short(&p.author),
                    "hasCursor": p.position.is_some(),
                })
            })
            .collect();
        let mut latency: Vec<Value> = c
            .latency
            .iter()
            .map(|(author, samples)| {
                let mut v = samples.clone();
                v.sort_unstable();
                json!({
                    "author": short(author),
                    "n": v.len(),
                    "p50Ms": percentile(&v, 50),
                    "p95Ms": percentile(&v, 95),
                })
            })
            .collect();
        latency.sort_by(|a, b| a["author"].as_str().cmp(&b["author"].as_str()));
        json!({
            "active": true,
            "doc": c.doc,
            "author": short(&c.author),
            "authorId": c.author,
            "joined": !c.awaiting_snapshot,
            "seq": c.seq,
            "peers": peers,
            "latency": latency,
            "remoteErrors": c.remote_errors,
            "unauthorized": c.unauthorized,
            "access": c.access.is_some(),
            "awaitingGrant": c.awaiting_grant,
            "realmShare": c.member_verifier.is_some(),
            "pending": c.pending.iter().map(|(node, (name, _, _, _))| json!({"node": short(node), "name": name})).collect::<Vec<_>>(),
            "contacts": c.contacts.len(),
        })
    }

    /// Shares this author's cursor when it moved since the last share.
    fn collab_send_cursor(&mut self) {
        let cursor = self.cursor_json();
        let Some(c) = self.collab.as_mut() else { return };
        if c.last_cursor == cursor {
            return;
        }
        let name = c.name.clone();
        c.last_cursor = cursor.clone();
        c.send(craft_collab::Kind::CursorMoved { position: cursor, name: Some(name) });
    }

    /// This author's cursor as the app encodes it: paragraph id + offset.
    fn cursor_json(&self) -> Option<String> {
        let f = &self.sel.focus;
        let p = self.doc.para_at(f)?;
        if p.pid == 0 {
            return None;
        }
        Some(json!({"pid": p.pid, "off": f.off.min(p.len())}).to_string())
    }

    /// Undoes this author's most recent local edit and shares the change.
    /// False when there is nothing of this author's to undo.
    pub(crate) fn collab_undo(&mut self) -> bool {
        let undone = match self.collab.as_mut() {
            Some(c) => c.texts.undo_local(),
            None => return false,
        };
        let Some((para, update, merged)) = undone else { return false };
        if let Some(c) = self.collab.as_mut() {
            c.send(craft_collab::Kind::TextEdit { para: para.clone(), update, props: None });
        }
        if let Ok(pid) = para.parse::<u64>()
            && self.doc.set_paragraph_text_by_pid(pid, &merged)
        {
            self.touch();
            self.clamp_selection();
        }
        if let Ok(pid) = para.parse::<u64>() {
            self.collab_sync_bold(pid);
        }
        true
    }

    /// The peers to draw, their cursors resolved to positions where known.
    pub fn collab_peers(&self) -> Vec<PeerView> {
        let Some(c) = self.collab.as_ref() else { return Vec::new() };
        c.presence
            .peers()
            .map(|p| PeerView { name: p.name.clone(), color: p.color, pos: p.position.as_deref().and_then(|s| cursor_pos(&self.doc, s)) })
            .collect()
    }
}

/// Whether a remote `Command` op may replay this command id: exactly the set
/// the local capture emits (not undo/redo/repeat, not typing or the anchored
/// bold, not the collab/debug/UI/file/tools/selection commands).
pub(crate) fn replayable_command(id: &str) -> bool {
    !(matches!(id, "edit.undo" | "edit.redo" | "edit.repeat" | "text.insert" | "format.bold")
        || id.starts_with("collab.")
        || id.starts_with("caret.")
        || id.starts_with("select.")
        || id.starts_with("view.")
        || id.starts_with("ui.")
        || id.starts_with("file.")
        || id.starts_with("tools."))
}

/// Decodes an app cursor (`{"pid": n, "off": n}`) into a position in `doc`.
fn cursor_pos(doc: &Document, encoded: &str) -> Option<Pos> {
    let v: Value = serde_json::from_str(encoded).ok()?;
    let pid = v.get("pid")?.as_u64()?;
    let off = v.get("off").and_then(Value::as_u64).unwrap_or(0) as usize;
    let (story, path) = doc.paragraph_by_pid(pid)?;
    let len = doc.para(story, &path).map(|p| p.len()).unwrap_or(0);
    Some(Pos { story, path, off: off.min(len) })
}

/// Decodes an app selection (`{"anchor": {"pid", "off"}, "focus": {...}}`)
/// into a session selection.
fn selection_from(doc: &Document, encoded: &str) -> Option<crate::Selection> {
    let v: Value = serde_json::from_str(encoded).ok()?;
    let end = |which: &str| -> Option<Pos> {
        let e = v.get(which)?;
        let pid = e.get("pid")?.as_u64()?;
        let off = e.get("off").and_then(Value::as_u64).unwrap_or(0) as usize;
        let (story, path) = doc.paragraph_by_pid(pid)?;
        let len = doc.para(story, &path).map(|p| p.len()).unwrap_or(0);
        Some(Pos { story, path, off: off.min(len) })
    };
    Some(crate::Selection { anchor: end("anchor")?, focus: end("focus")? })
}

/// The first eight hex characters of an author id, for reports.
pub(crate) fn short(author: &str) -> String {
    author.chars().take(8).collect()
}

/// The hex form of bytes (keys, ids).
fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The p-th percentile of a sorted, non-empty sample list (nearest rank).
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * p) / 100;
    sorted[idx.min(sorted.len() - 1)]
}

impl craft_collab::DocAdapter for Session {
    fn snapshot(&self) -> Vec<u8> {
        serde_json::to_vec(&self.doc).unwrap_or_default()
    }

    fn load_snapshot(&mut self, bytes: &[u8]) -> Result<(), String> {
        let doc: Document = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        self.set_document(doc);
        Ok(())
    }

    fn paragraph(&self, para: &str) -> Option<String> {
        let pid = para.parse::<u64>().ok()?;
        let (story, path) = self.doc.paragraph_by_pid(pid)?;
        self.doc.para(story, &path).map(|p| p.text.clone())
    }

    fn set_paragraph(&mut self, para: &str, text: &str) {
        if let Ok(pid) = para.parse::<u64>()
            && self.doc.set_paragraph_text_by_pid(pid, text)
        {
            self.touch();
        }
    }
}

impl craft_collab::CollabHost for Session {
    fn dispatch(&mut self, id: &str, params: &Value, origin: craft_collab::Origin) -> Result<Value, String> {
        self.run_with_origin(id, params, origin).map_err(|e: CmdError| e.to_string())
    }
}

/// Where a local position in a paragraph lands after a remote splice there
/// (`pre` bytes kept, `removed` replaced by `inserted`): before the splice
/// or exactly at an insert it stays (the remote text lands before the
/// caret, so each writer's sentence stays whole); after it, it moves by the
/// change; inside a removed span, it goes to the span's start.
pub(crate) fn shifted_off(off: usize, pre: usize, removed: usize, inserted: usize) -> usize {
    if off <= pre {
        off
    } else if off >= pre + removed {
        off - removed + inserted
    } else {
        pre
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_collab::MemoryTransport;
    use wordcraft_doc::StoryRef;

    fn session() -> Session {
        Session::new(Document::from_text(""))
    }

    /// Found in the three-person demo: a remote insert before the local
    /// caret left the caret at the same absolute offset, so the local
    /// person's next words landed inside the other person's text. The local
    /// caret follows remote edits made before it; a remote insert exactly at
    /// it lands before it, so each person's sentence stays contiguous.
    #[test]
    fn the_local_caret_follows_remote_edits_made_before_it() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();
        a.run("text.insert", &json!({"text": "Field notes. Third line."})).expect("A types");
        b.collab_tick();

        // Bob types at the end, word by word; Alice types after "Field notes. ".
        let end = b.doc.plain_text(StoryRef::Body).len();
        b.run("caret.set", &json!({"pos": {"story": "body", "path": [0], "off": end}})).expect("B caret");
        a.run("caret.set", &json!({"pos": {"story": "body", "path": [0], "off": 13}})).expect("A caret");
        for (wa, wb) in [("No ", " Each"), ("server ", " copy"), ("here. ", " converges.")] {
            a.run("text.insert", &json!({"text": wa})).expect("A types a word");
            b.run("text.insert", &json!({"text": wb})).expect("B types a word");
            a.collab_tick();
            b.collab_tick();
            a.collab_tick();
        }
        let text = a.doc.plain_text(StoryRef::Body);
        assert_eq!(text, b.doc.plain_text(StoryRef::Body), "converged");
        assert_eq!(text, "Field notes. No server here. Third line. Each copy converges.", "each sentence stayed whole");
    }

    /// Where a position lands after a remote splice: before it, unchanged;
    /// after it, moved by the change; inside a removed span, its start.
    #[test]
    fn a_position_is_shifted_by_a_remote_splice() {
        use super::shifted_off;
        // An insert of 4 at 5: before, at, after.
        assert_eq!(shifted_off(3, 5, 0, 4), 3);
        assert_eq!(shifted_off(5, 5, 0, 4), 5, "an insert at the caret lands before it");
        assert_eq!(shifted_off(9, 5, 0, 4), 13);
        // A removal of 3 at 5 (replaced by 1): inside goes to the start.
        assert_eq!(shifted_off(6, 5, 3, 1), 5);
        assert_eq!(shifted_off(10, 5, 3, 1), 8);
    }

    #[test]
    fn remote_typing_converges_and_stays_out_of_undo() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        // Join handshake: B asks for the document, A answers with it.
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), a.doc.plain_text(StoryRef::Body), "B joined A's document");

        a.run("text.insert", &json!({"text": "Hello "})).expect("A types");
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hello ");
        b.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), "Hello ", "A's typing reached B");
        assert!(!b.can_undo(), "a remote edit must not enter the local undo history");

        b.run("text.insert", &json!({"text": "world"})).expect("B types");
        a.collab_tick();
        let merged = a.doc.plain_text(StoryRef::Body);
        assert_eq!(merged, b.doc.plain_text(StoryRef::Body), "both sides converge: {merged:?}");
        assert!(merged.contains("Hello") && merged.contains("world"), "neither edit was lost: {merged:?}");

        // Presence: A's cursor is a peer in B and resolves to a position.
        b.collab_tick();
        let peers = b.collab_peers();
        assert_eq!(peers.len(), 1, "A's cursor is a peer in B");
        assert!(peers[0].pos.is_some(), "the peer cursor resolves to a position in the document");

        // A undoes: only A's own text goes; B's survives on both sides.
        assert!(a.undo(), "A has its own edit to undo");
        b.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "world", "only A's text was taken back");
        assert_eq!(b.doc.plain_text(StoryRef::Body), "world");

        // B undoes too: now only B's text goes.
        assert!(b.undo(), "B has its own edit to undo");
        a.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "");
        assert_eq!(b.doc.plain_text(StoryRef::Body), "");
    }

    #[test]
    fn demo_sequence_keeps_per_author_undo() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Hello from Alice. "})).expect("A types");
        b.collab_tick();
        b.run("text.insert", &json!({"text": "Hi from Bob. "})).expect("B types");
        a.collab_tick();
        b.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), b.doc.plain_text(StoryRef::Body));

        // A undoes through the command, exactly as the app does.
        a.run("edit.undo", &json!({})).expect("A undoes");
        b.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hi from Bob. ", "only A's text went");
        assert_eq!(b.doc.plain_text(StoryRef::Body), "Hi from Bob. ");

        // B types again, after hearing A's undo.
        b.run("text.insert", &json!({"text": "XYZ. "})).expect("B types again");
        a.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hi from Bob. XYZ. ", "B's typing reached A");
        assert_eq!(b.doc.plain_text(StoryRef::Body), "Hi from Bob. XYZ. ");

        // And B's undo takes back only B's text.
        b.run("edit.undo", &json!({})).expect("B undoes");
        a.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), "Hi from Bob. ", "only B's text went");
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hi from Bob. ");
    }

    #[test]
    fn a_command_op_replays_with_its_selection() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Hello world"})).expect("A types");
        b.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), "Hello world");

        // B bolds "world": the op carries B's selection, and A replays it.
        b.run("select.text", &json!({"text": "world"})).expect("B selects");
        b.run("format.bold", &json!({})).expect("B bolds");
        a.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hello world");
        let bold = |s: &Session, off: usize| s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(0)).and_then(|p| p.props_of_char(off).bold);
        assert_eq!(bold(&b, 6), Some(true), "B bolded 'world'");
        assert_eq!(bold(&a, 6), Some(true), "A replayed the bold");
        assert_eq!(bold(&a, 0), None, "the text before it is untouched");
        assert_eq!(bold(&a, 10), Some(true), "the whole word is bold");
    }

    fn runs_of(s: &Session) -> Vec<(usize, usize, wordcraft_doc::CharProps)> {
        let Some(p) = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(0)) else { return Vec::new() };
        let mut out = Vec::new();
        let mut start = 0;
        for r in &p.runs {
            out.push((start, start + r.len, r.props.clone()));
            start += r.len;
        }
        out
    }

    #[test]
    fn style_predating_a_remote_text_update_survives() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Hello world. "})).expect("A types");
        b.collab_tick();
        // B bolds "world"; A replays it.
        b.run("select.text", &json!({"text": "world"})).expect("B selects");
        b.run("format.bold", &json!({})).expect("B bolds");
        a.collab_tick();
        assert_eq!(runs_of(&a), runs_of(&b), "the bold replicated");

        // A types after the bold: B's bold must survive the text update.
        a.run("text.insert", &json!({"text": "more "})).expect("A types again");
        b.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), b.doc.plain_text(StoryRef::Body));
        assert_eq!(runs_of(&a), runs_of(&b), "style predating the text update survived on B");
    }

    #[test]
    fn replaying_a_remote_command_keeps_the_local_caret() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Hello world"})).expect("A types");
        b.collab_tick();
        let before = a.sel.clone();

        // B applies a position-carrying command (italic, still replayed as a
        // Command op); A replays it, but must not keep B's selection.
        b.run("select.text", &json!({"text": "Hello"})).expect("B selects");
        b.run("format.italic", &json!({})).expect("B italicises");
        a.collab_tick();
        assert_eq!(a.sel, before, "A's own caret was restored after the replay");

        // A's next typing goes at its caret, not over the remote selection.
        a.run("text.insert", &json!({"text": "X"})).expect("A types");
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hello worldX", "typed at the caret");
    }

    /// Formatting one author applies reaches the others with the same
    /// coverage: bold through the CRDT, italic, colour and highlight as
    /// replayed commands under the author's own selection.
    #[test]
    fn formatting_from_one_author_lands_on_the_other() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();
        a.run("text.insert", &json!({"text": "Hello wide world"})).expect("A types");
        b.collab_tick();

        b.run("select.text", &json!({"text": "Hello"})).expect("B selects");
        b.run("format.bold", &json!({})).expect("B bolds");
        b.run("select.text", &json!({"text": "wide"})).expect("B selects");
        b.run("format.italic", &json!({})).expect("B italicises");
        b.run("format.color", &json!({"color": "C0392B"})).expect("B colours");
        b.run("select.text", &json!({"text": "world"})).expect("B selects");
        b.run("format.highlight", &json!({"color": "yellow"})).expect("B highlights");
        a.collab_tick();
        b.collab_tick();
        a.collab_tick();

        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hello wide world");
        assert_eq!(runs_of(&a), runs_of(&b), "the same runs on both sides");
        let para = a.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(0)).expect("paragraph");
        assert_eq!(para.props_of_char(0).bold, Some(true), "bold arrived");
        assert_eq!(para.props_of_char(6).italic, Some(true), "italic arrived");
        assert!(para.props_of_char(6).color.is_some(), "colour arrived");
        assert!(para.props_of_char(11).highlight.is_some(), "highlight arrived");
        assert_eq!(para.props_of_char(11).italic, None, "and only where it was applied");
    }

    #[test]
    fn a_restarted_joiner_catches_up_and_keeps_merging() {
        // All three share one bus, as they do on one mesh topic; Carol's
        // session is fresh (a restarted process) and joins existing content.
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb", "cccccccc"]);
        let (ta, tb, tc) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut a = session();
        let mut b = session();
        let mut c = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Hello "})).expect("A types");
        b.collab_tick();
        a.run("text.insert", &json!({"text": "world. "})).expect("A types again");
        b.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), "Hello world. ");

        // Carol restarts and joins the existing document.
        c.collab_join("demo", "Carol", Box::new(tc));
        c.collab_tick();
        a.collab_tick();
        b.collab_tick();
        c.collab_tick();
        assert_eq!(c.doc.plain_text(StoryRef::Body), "Hello world. ", "Carol caught up");

        // Both directions keep merging after the join.
        c.run("text.insert", &json!({"text": "C! "})).expect("C types");
        a.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), c.doc.plain_text(StoryRef::Body), "C's typing reached A");
        a.run("text.insert", &json!({"text": "A2 "})).expect("A types");
        c.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), c.doc.plain_text(StoryRef::Body), "A's typing reached C");
        b.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), a.doc.plain_text(StoryRef::Body), "B converges too");
    }

    #[test]
    fn a_racing_bold_covers_the_same_text_on_every_replica() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();

        a.run("text.insert", &json!({"text": "Bravo. Alpha. "})).expect("A types");
        b.collab_tick();

        // The race: B bolds "Alpha." while A inserts "ONE " inside it,
        // neither having heard the other yet.
        b.run("select.text", &json!({"text": "Alpha."})).expect("B selects");
        b.run("format.bold", &json!({})).expect("B bolds");
        a.run("caret.set", &json!({"pos": {"story": "body", "path": [0], "off": 10}})).expect("A caret");
        a.run("text.insert", &json!({"text": "ONE "})).expect("A inserts");

        // Exchange both ways; then everything settles.
        a.collab_tick();
        b.collab_tick();
        a.collab_tick();

        assert_eq!(a.doc.plain_text(StoryRef::Body), b.doc.plain_text(StoryRef::Body), "text converged");
        assert_eq!(runs_of(&a), runs_of(&b), "runs (coverage and segmentation) equal: the bold is anchored");

        // Both sides type again at their own carets, as they did while Carol
        // was partitioned: alignment must hold across further merges.
        a.run("text.insert", &json!({"text": "PA. "})).expect("A types again");
        b.run("text.insert", &json!({"text": "PB. "})).expect("B types again");
        a.collab_tick();
        b.collab_tick();
        a.collab_tick();
        assert_eq!(a.doc.plain_text(StoryRef::Body), b.doc.plain_text(StoryRef::Body), "text converged after more typing");
        assert_eq!(runs_of(&a), runs_of(&b), "runs stayed aligned after more typing");
    }

    /// A verifier standing in for the adapter's key check: these engine
    /// tests exercise the session's enforcement, while the install path's
    /// crypto is exercised in the adapter's own tests.
    struct TestVerifier;

    impl craft_collab::GrantVerifier for TestVerifier {
        fn verify(&self, signed: &craft_collab::SignedGrant, _now_ms: u64) -> Result<craft_collab::Grant, String> {
            Ok(signed.grant.clone())
        }

        fn verify_revocation(&self, signed: &craft_collab::SignedRevocation, _now_ms: u64) -> Result<craft_collab::Revocation, String> {
            Ok(signed.revocation.clone())
        }
    }

    /// A stand-in realm-membership verifier: one accepted UCAN string, one
    /// role, and when the membership ends (None: no end). The real
    /// verification is the adapter's.
    struct TestMemberVerifier {
        accepts: String,
        role: craft_collab::Role,
        ends_ms: Option<u64>,
    }

    impl craft_collab::MembershipVerifier for TestMemberVerifier {
        fn verify_member(&self, ucan: &str, node_id: &str, _now_ms: u64) -> Result<u64, String> {
            if node_id.is_empty() {
                return Err("no node".into());
            }
            if ucan == self.accepts { Ok(self.ends_ms.unwrap_or(u64::MAX)) } else { Err("not a member".into()) }
        }

        fn role(&self) -> craft_collab::Role {
            self.role
        }

        fn realm(&self) -> &str {
            "test.realm-e"
        }
    }

    /// A host that mints real grant structures for the join-flow tests; the
    /// crypto itself is exercised in the adapter's tests.
    struct TestIssuer {
        host: String,
    }

    impl craft_collab::GrantIssuer for TestIssuer {
        fn node_id(&self) -> String {
            self.host.clone()
        }

        fn carried_key(&self) -> Vec<u8> {
            vec![7; 32]
        }

        fn issue(&self, doc: &str, holder: &str, role: craft_collab::Role, ttl_ms: u64, now_ms: u64) -> Result<craft_collab::SignedGrant, String> {
            Ok(craft_collab::SignedGrant {
                grant: craft_collab::Grant {
                    v: craft_collab::GRANT_VERSION,
                    doc: doc.to_string(),
                    host: self.host.clone(),
                    holder: holder.to_string(),
                    role,
                    issued_ms: now_ms,
                    expires_ms: now_ms.saturating_add(ttl_ms),
                    nonce: craft_collab::mint_nonce(now_ms, 1),
                },
                signature: vec![],
            })
        }

        fn revoke(&self, doc: &str, holder: &str, now_ms: u64) -> Result<craft_collab::SignedRevocation, String> {
            Ok(craft_collab::SignedRevocation {
                revocation: craft_collab::Revocation {
                    v: craft_collab::GRANT_VERSION,
                    doc: doc.to_string(),
                    host: self.host.clone(),
                    holder: holder.to_string(),
                    issued_ms: now_ms,
                    nonce: craft_collab::mint_nonce(now_ms, 1),
                },
                signature: vec![],
            })
        }

        fn open_token(&self, doc: &str, role: craft_collab::Role, ttl_ms: u64, now_ms: u64) -> Result<craft_collab::SignedGrant, String> {
            self.issue(doc, "", role, ttl_ms, now_ms)
        }

        fn sign(&self, _bytes: &[u8]) -> Result<Vec<u8>, String> {
            Ok(b"isig".to_vec())
        }
    }

    fn host_table() -> craft_collab::Access {
        craft_collab::Access::new("demo", "aaaaaaaa", std::sync::Arc::new(TestVerifier))
    }

    fn token(host: &str, role: craft_collab::Role) -> craft_collab::SignedGrant {
        craft_collab::GrantIssuer::open_token(&TestIssuer { host: host.to_string() }, "demo", role, 60_000, now_ms()).expect("token")
    }

    #[test]
    fn a_join_with_an_invite_is_granted_and_receives_the_document() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.run("text.insert", &json!({"text": "hello "})).expect("Host types");

        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
        bob.collab_set_access(host_table());

        bob.collab_tick(); // the join request goes out
        host.collab_tick(); // the host admits: the grant is issued
        bob.collab_tick(); // the grant lands here: the snapshot request goes out
        host.collab_tick(); // the host answers the snapshot
        bob.collab_tick(); // the snapshot lands

        assert_eq!(bob.collab_stats()["joined"], true, "the joiner is admitted end to end");
        assert_eq!(bob.doc.plain_text(StoryRef::Body), "hello ");

        bob.run("caret.set", &json!({"pos": {"story": "body", "path": [0], "off": 6}})).expect("Bob's caret");
        bob.run("text.insert", &json!({"text": "world"})).expect("Bob types back");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "hello world", "a granted joiner edits the shared doc");
        bob.collab_tick();
        assert_eq!(bob.doc.plain_text(StoryRef::Body), "hello world");
    }

    #[test]
    fn a_join_request_with_a_token_for_another_host_is_refused() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);

        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("cccccccc", craft_collab::Role::Editor));
        bob.collab_set_access(host_table());
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();

        // The door's refusal is counted, and the joiner's cursor message is
        // dropped as an unknown holder on top.
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1, "the forged token is refused at the door");
        assert_eq!(bob.collab_stats()["awaitingGrant"], true, "the joiner is still waiting");
        assert_eq!(bob.doc.plain_text(StoryRef::Body), "", "and received no document");
    }

    #[test]
    fn the_host_approves_a_pending_joiner_by_hand() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        // auto_approve stays off: this is the UI's path.

        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
        bob.collab_set_access(host_table());
        bob.collab_tick();
        host.collab_tick();

        let pending = host.collab_pending();
        assert_eq!(pending.len(), 1, "the request waits for a yes");
        assert_eq!(pending[0], ("bbbbbbbb".to_string(), "Bob".to_string()));

        host.collab_approve("bbbbbbbb").expect("approve");
        assert!(host.collab_pending().is_empty(), "approved, no longer pending");
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();
        assert_eq!(bob.collab_stats()["joined"], true, "the hand-approved joiner lands");
    }

    #[test]
    fn join_requests_land_in_the_contacts_and_survive_a_restart() {
        let file = std::env::temp_dir().join(format!("wordcraft-contacts-{}-{}.json", std::process::id(), now_ms()));
        let path = file.to_str().expect("temp path");
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_contacts_path(path);
        // auto_approve stays off: this joiner waits, and is remembered anyway.

        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
        bob.collab_set_access(host_table());
        bob.collab_tick();
        host.collab_tick();

        let contacts = host.collab_contacts();
        assert_eq!(contacts.len(), 1, "the waiting joiner is remembered");
        assert_eq!(contacts[0].node, "bbbbbbbb");
        assert_eq!(contacts[0].name, "Bob");
        assert_eq!(host.collab_pending().len(), 1, "and still waits for the yes");

        host.collab_approve("bbbbbbbb").expect("approve");
        assert_eq!(host.collab_contacts().len(), 1, "approving does not duplicate the person");
        assert!(host.collab_pending().is_empty());

        // The joiner remembers its host, realm and all.
        bob.collab_note_contact("aaaaaaaa", "Host", Some("io.macula"));
        let bobs = bob.collab_contacts();
        assert_eq!(bobs[0].name, "Host");
        assert_eq!(bobs[0].realm.as_deref(), Some("io.macula"));

        // A restarted host reads the same people back from the same file.
        let mut restarted = session();
        restarted.collab_attach("demo", "Host", Box::new(MemoryTransport::bus(&["aaaaaaaa"]).remove(0)));
        restarted.collab_set_contacts_path(path);
        let back = restarted.collab_contacts();
        assert_eq!(back.len(), 1, "contacts persist across restarts");
        assert_eq!(back[0].name, "Bob");

        assert!(restarted.collab_forget_contact("bbbbbbbb"));
        assert!(restarted.collab_contacts().is_empty());
        let text = std::fs::read_to_string(&file).expect("the file is there");
        assert!(!text.contains("Bob"), "the write-through removed the person");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn revoking_a_holder_stops_their_edits_everywhere() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
        bob.collab_set_access(host_table());
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();
        assert_eq!(bob.collab_stats()["joined"], true);

        host.collab_revoke("bbbbbbbb").expect("revoke");
        bob.collab_tick(); // the joiner applies its own revocation
        host.collab_tick();

        bob.run("text.insert", &json!({"text": "still here "})).expect("Bob types");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "", "a revoked holder no longer lands");
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1);
    }

    #[test]
    fn a_minted_invite_carries_the_hosts_signature() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, _) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_share_context("cc".repeat(32).as_str(), "", "station.example:4433:dd", "pq_pure");

        let line = host.collab_invite_from_context(craft_collab::Role::Editor, 60_000).expect("mint");
        let invite = craft_collab::Invite::decode(&line).expect("decode");
        assert_eq!(invite.v, craft_collab::INVITE_VERSION, "current version only");
        assert!(!invite.signature.is_empty(), "the envelope is signed by the host");
        assert_eq!(invite.host, "aaaaaaaa");
        assert!(invite.is_live(now_ms()));
        assert!(invite.token.as_ref().expect("token").grant.is_open(), "still an open token");

        // And minting without a signing side is refused, never unsigned.
        let mut bare = session();
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (tb, _) = (bus.remove(0), bus.remove(0));
        bare.collab_attach("demo", "Host", Box::new(tb));
        bare.collab_set_share_context("cc", "", "seed", "pq_pure");
        assert!(bare.collab_invite_from_context(craft_collab::Role::Editor, 60_000).is_err());
    }

    #[test]
    fn a_realm_member_joins_through_the_membership_door() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bea = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.collab_set_member_verifier(Box::new(TestMemberVerifier {
            accepts: "member-good".into(),
            role: craft_collab::Role::Viewer,
            ends_ms: None,
        }));
        host.run("text.insert", &json!({"text": "shared "})).expect("Host types");

        // The joiner has no invite token at all: a realm membership is its
        // only admission.
        bea.collab_join_requesting("demo", "Bea", Box::new(tb), None, Some("member-good".into()));
        bea.collab_set_access(host_table());
        bea.collab_tick(); // the membership goes out
        host.collab_tick(); // the door verifies it
        bea.collab_tick(); // the grant lands: ask for the snapshot
        host.collab_tick();
        bea.collab_tick();

        assert_eq!(bea.collab_stats()["joined"], true, "a realm member lands");
        assert_eq!(bea.doc.plain_text(StoryRef::Body), "shared ", "and reads the document");
        assert_eq!(host.collab_holders(), vec!["bbbbbbbb".to_string()]);

        // The mapped role is Viewer: writing is refused on every op.
        bea.run("text.insert", &json!({"text": "nope "})).expect("Bea types");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "shared ", "a viewer's edit does not land");
        assert_eq!(host.collab_stats()["unauthorized"], 1);
    }

    /// Cross-realm: a host signed in to realm H shares with realm E's
    /// members. Its realm invite names E as the member realm (what the
    /// joiner presents a membership of) and H as the host's own realm.
    #[test]
    fn a_realm_invite_names_the_shared_realm_and_the_hosts_own_realm() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa"]);
        let mut host = session();
        host.collab_attach("demo", "Host", Box::new(bus.remove(0)));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_realm(json!({"signedIn": true, "realm": "test.realm-h"}));
        host.collab_set_member_verifier(Box::new(TestMemberVerifier {
            accepts: "member-good".into(),
            role: craft_collab::Role::Editor,
            ends_ms: None,
        }));
        let line = host.collab_mint_realm_invite(&"cc".repeat(32), "", "station.example:4433:dd", "pq_hybrid", 60_000).expect("a realm invite");
        let invite = craft_collab::Invite::decode(&line).expect("decode");
        assert_eq!(invite.member_realm.as_deref(), Some("test.realm-e"), "the joiner presents an E membership");
        assert_eq!(invite.host_realm.as_deref(), Some("test.realm-h"), "the host is attested in H");
        assert_eq!(invite.member_role, Some(craft_collab::Role::Editor));
        assert!(invite.token.is_none());

        // A host not signed in to any realm names none; a token invite names
        // no member realm.
        host.collab_set_realm(json!({"signedIn": false}));
        let line = host
            .collab_mint_invite(&"cc".repeat(32), "", "station.example:4433:dd", "pq_hybrid", craft_collab::Role::Editor, 60_000)
            .expect("an invite");
        let invite = craft_collab::Invite::decode(&line).expect("decode");
        assert_eq!(invite.host_realm, None);
        assert_eq!(invite.member_realm, None);
    }

    #[test]
    fn a_bogus_membership_is_refused_at_a_realm_door() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bea = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.collab_set_member_verifier(Box::new(TestMemberVerifier {
            accepts: "member-good".into(),
            role: craft_collab::Role::Editor,
            ends_ms: None,
        }));

        bea.collab_join_requesting("demo", "Mallory", Box::new(tb), None, Some("member-bad".into()));
        bea.collab_set_access(host_table());
        bea.collab_tick();
        host.collab_tick();
        bea.collab_tick();

        // The door's refusal is counted, and the joiner's cursor message is
        // dropped as an unknown holder on top.
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1, "the door refused the forged membership");
        assert_eq!(bea.collab_stats()["awaitingGrant"], true, "and the joiner is still waiting");
        assert_eq!(bea.doc.plain_text(StoryRef::Body), "", "nothing arrived");
    }

    /// A realm member's grant never outlives their membership: the door caps
    /// it at the UCAN's own end, so a revoked or lapsed membership takes the
    /// document access down with it (the cascade).
    #[test]
    fn a_member_grant_never_outlives_the_membership() {
        let file = std::env::temp_dir().join(format!("wordcraft-member-grant-{}-{}.json", std::process::id(), now_ms()));
        let path = file.to_str().expect("temp path");
        let membership_end = now_ms() + 60_000;
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bea = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_grants_path(path);
        host.collab_set_member_verifier(Box::new(TestMemberVerifier {
            accepts: "member-ends-soon".into(),
            role: craft_collab::Role::Editor,
            ends_ms: Some(membership_end),
        }));

        bea.collab_join_requesting("demo", "Bea", Box::new(tb), None, Some("member-ends-soon".into()));
        bea.collab_set_access(host_table());
        bea.collab_tick();
        host.collab_tick();
        assert_eq!(host.collab_pending().len(), 1, "the member waits for the yes");

        host.collab_approve("bbbbbbbb").expect("approve");
        let text = std::fs::read_to_string(&file).expect("the grant persisted");
        let grants: Vec<craft_collab::SignedGrant> = serde_json::from_str(&text).expect("grants");
        let grant = grants.first().expect("one grant");
        assert_eq!(grant.grant.role, craft_collab::Role::Editor, "the verifier's role");
        assert!(grant.grant.expires_ms <= membership_end, "the grant does not outlive the membership");
        assert!(grant.grant.expires_ms > now_ms(), "but is live now");
        let _ = std::fs::remove_file(&file);
    }

    /// A membership that has already ended is not an admission: the door
    /// refuses it like any other stranger.
    #[test]
    fn a_membership_that_has_ended_admits_nobody() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bea = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.collab_set_member_verifier(Box::new(TestMemberVerifier {
            accepts: "member-ended".into(),
            role: craft_collab::Role::Editor,
            ends_ms: Some(now_ms().saturating_sub(1)),
        }));

        bea.collab_join_requesting("demo", "Bea", Box::new(tb), None, Some("member-ended".into()));
        bea.collab_set_access(host_table());
        bea.collab_tick();
        host.collab_tick();
        bea.collab_tick();

        assert!(host.collab_pending().is_empty(), "nothing waits for a yes");
        assert_eq!(host.collab_holders().len(), 0, "no grant was issued");
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1, "and the refusal is counted");
        assert_eq!(bea.collab_stats()["awaitingGrant"], true, "the joiner is still waiting");
    }

    /// An invite minted for one person admits only that node: the token's
    /// holder is checked against the message's verified author at the door.
    #[test]
    fn an_invite_for_one_person_admits_only_that_node() {
        // Node ids in the wild are 64 hex characters; the invite's person is
        // named by exactly the id the transport reports as its author.
        let bob_id = "bb".repeat(32);
        let carol_id = "cc".repeat(32);
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", bob_id.as_str(), carol_id.as_str()]);
        let (ta, tb, tc) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        let mut carol = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.run("text.insert", &json!({"text": "for one "})).expect("Host types");

        let realm = "cc".repeat(32);
        let line = host
            .collab_mint_invite_for(&realm, "", "station.example:4433:dd", "pq_pure", &bob_id, "Bob", craft_collab::Role::Editor, 60_000)
            .expect("mint for Bob");
        let invite = craft_collab::Invite::decode(&line).expect("decode");
        assert_eq!(invite.invitee.as_deref(), Some(bob_id.as_str()));
        assert_eq!(invite.invitee_name.as_deref(), Some("Bob"));
        let token = invite.token.clone().expect("a pre-named token");
        assert_eq!(token.grant.holder, bob_id, "the token names its person");

        // Carol presents Bob's invite: the door refuses her.
        carol.collab_join_via_invite("demo", "Carol", Box::new(tc), token.clone());
        carol.collab_set_access(host_table());
        carol.collab_tick();
        host.collab_tick();
        carol.collab_tick();
        assert_eq!(carol.collab_stats()["awaitingGrant"], true, "another node's invite does not admit");
        assert_eq!(carol.doc.plain_text(StoryRef::Body), "", "and nothing arrived");
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1, "the refusal is counted");

        // Bob presents his own invite: admitted with the document.
        bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token);
        bob.collab_set_access(host_table());
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();
        assert_eq!(bob.collab_stats()["joined"], true, "the named node is admitted");
        assert_eq!(bob.doc.plain_text(StoryRef::Body), "for one ");
    }

    #[test]
    fn a_restarted_host_relearns_its_holders() {
        let dir = std::env::temp_dir().join(format!("wc-grants-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("grants.json");
        let _ = std::fs::remove_file(&path);
        let file = path.to_string_lossy().to_string();

        // The first host: Bob joins, the grant lands in the file.
        {
            let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
            let (ta, tb) = (bus.remove(0), bus.remove(0));
            let mut host = session();
            let mut bob = session();
            host.collab_attach("demo", "Host", Box::new(ta));
            host.collab_set_access(host_table());
            host.collab_set_grants_path(&file);
            host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
            host.collab_set_auto_approve(true);
            bob.collab_join_via_invite("demo", "Bob", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
            bob.collab_set_access(host_table());
            bob.collab_tick();
            host.collab_tick();
            bob.collab_tick();
            assert_eq!(host.collab_holders(), vec!["bbbbbbbb".to_string()]);
        }

        // A second, fresh host process: the holders come back from the file.
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, _) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_grants_path(&file);
        assert_eq!(host.collab_holders(), vec!["bbbbbbbb".to_string()], "the restarted host re-learns its holders");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_refused_command_is_dropped_on_the_receive_path() {
        use craft_collab::Transport as _;
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, mut tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(verified_table("aaaaaaaa", "bbbbbbbb", craft_collab::Role::Editor, u64::MAX));

        // The holder skips its own capture and puts a raw, non-replayable
        // command on the wire, as a modified peer would.
        tb.send(craft_collab::Message {
            doc: "demo".into(),
            author: "bbbbbbbb".into(),
            epoch: 1,
            seq: 1,
            sent_at_ms: now_ms(),
            kind: craft_collab::Kind::Command {
                id: "file.open".into(),
                params: serde_json::to_vec(&json!({"path": "x"})).expect("json"),
                anchor: None,
            },
        })
        .expect("send");
        host.collab_tick();
        assert_eq!(host.collab_stats()["unauthorized"], 1, "the refused command is dropped, not applied");

        // The same holder's replayable command still applies.
        tb.send(craft_collab::Message {
            doc: "demo".into(),
            author: "bbbbbbbb".into(),
            epoch: 1,
            seq: 2,
            sent_at_ms: now_ms(),
            kind: craft_collab::Kind::Command { id: "text.newParagraph".into(), params: serde_json::to_vec(&json!({})).expect("json"), anchor: None },
        })
        .expect("send");
        host.collab_tick();
        assert_eq!(host.collab_stats()["unauthorized"], 1, "the replayable command is not dropped");
        assert_eq!(host.doc.para_paths(StoryRef::Body).len(), 2, "and it applied: the paragraph split");
    }

    /// The race the automated demo found live: two joiners are approved at
    /// the same time; the first has not caught up yet when the second asks
    /// for the document. Its empty document must not win — only the host
    /// answers, and the second joiner lands on the real text.
    #[test]
    fn a_joining_peer_with_nothing_yet_does_not_answer_snapshots() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb", "cccccccc"]);
        let (ta, tb, tc) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut b = session();
        let mut c = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(host_table());
        host.collab_set_issuer(Box::new(TestIssuer { host: "aaaaaaaa".into() }));
        host.collab_set_auto_approve(true);
        host.run("text.insert", &json!({"text": "real "})).expect("Host types");

        b.collab_join_via_invite("demo", "Bea", Box::new(tb), token("aaaaaaaa", craft_collab::Role::Editor));
        b.collab_set_access(host_table());
        c.collab_join_via_invite("demo", "Cy", Box::new(tc), token("aaaaaaaa", craft_collab::Role::Editor));
        c.collab_set_access(host_table());

        b.collab_tick(); // both ask for admission
        c.collab_tick();
        host.collab_tick(); // the host admits both
        b.collab_tick(); // b gets its grant and asks for the document
        c.collab_tick(); // c gets its grant and asks too
        b.collab_tick(); // b now sees c's request while still waiting itself

        host.collab_tick(); // the host answers c (and b) with the real document
        c.collab_tick(); // c loads: b must not have answered emptily

        assert_eq!(c.doc.plain_text(StoryRef::Body), "real ", "the empty joiner's snapshot must not win the race");
        assert_eq!(c.collab_stats()["joined"], true);
        b.collab_tick();
        assert_eq!(b.doc.plain_text(StoryRef::Body), "real ");
    }

    #[test]
    fn a_holder_answers_a_joiners_snapshot_not_only_the_host() {
        // The joiner's only reachable peer is another holder: catch-up must
        // not depend on the host being present.
        let mut bus = MemoryTransport::bus(&["bbbbbbbb", "cccccccc"]);
        let (tb, tc) = (bus.remove(0), bus.remove(0));
        let mut bea = session();
        bea.collab_attach("demo", "Bea", Box::new(tb));
        bea.run("text.insert", &json!({"text": "carried "})).expect("Bea types");
        let table = || {
            let mut a = craft_collab::Access::new("demo", "aaaaaaaa", std::sync::Arc::new(TestVerifier));
            for (holder, role) in [("bbbbbbbb", craft_collab::Role::Editor), ("cccccccc", craft_collab::Role::Editor)] {
                a.install(
                    &craft_collab::SignedGrant {
                        grant: craft_collab::Grant {
                            v: craft_collab::GRANT_VERSION,
                            doc: "demo".into(),
                            host: "aaaaaaaa".into(),
                            holder: holder.into(),
                            role,
                            issued_ms: 0,
                            expires_ms: u64::MAX,
                            nonce: format!("n-{holder}"),
                        },
                        signature: vec![],
                    },
                    0,
                )
                .expect("install");
            }
            a
        };
        bea.collab_set_access(table());

        let mut cy = session();
        cy.collab_join("demo", "Cy", Box::new(tc));
        cy.collab_set_access(table());
        cy.collab_tick(); // the request reaches the holder
        bea.collab_tick(); // the holder answers
        cy.collab_tick(); // the joiner loads it

        assert_eq!(cy.collab_stats()["joined"], true, "a holder carries the joiner");
        assert_eq!(cy.doc.plain_text(StoryRef::Body), "carried ");
    }

    fn verified_table(host: &str, holder: &str, role: craft_collab::Role, expires_ms: u64) -> craft_collab::Access {
        let mut a = craft_collab::Access::new("demo", host, std::sync::Arc::new(TestVerifier));
        a.install(
            &craft_collab::SignedGrant {
                grant: craft_collab::Grant {
                    v: craft_collab::GRANT_VERSION,
                    doc: "demo".into(),
                    host: host.into(),
                    holder: holder.into(),
                    role,
                    issued_ms: 0,
                    expires_ms,
                    nonce: format!("n-{holder}"),
                },
                signature: vec![],
            },
            0,
        )
        .expect("install");
        a
    }

    #[test]
    fn a_stranger_cannot_edit_a_protected_document() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb", "cccccccc"]);
        let (ta, tb, tm) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        let mut mallory = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        bob.collab_join("demo", "Bob", Box::new(tb));
        mallory.collab_attach("demo", "Mallory", Box::new(tm));
        let table = || verified_table("aaaaaaaa", "bbbbbbbb", craft_collab::Role::Editor, u64::MAX);
        host.collab_set_access(table());
        bob.collab_set_access(table());
        bob.collab_tick();
        host.collab_tick();
        bob.collab_tick();

        // Mallory holds no grant: her edit must not land.
        mallory.run("text.insert", &json!({"text": "evil "})).expect("Mallory types");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "", "the stranger's edit was dropped");
        assert_eq!(host.collab_stats()["unauthorized"], 1);

        // Bob is a live editor: his edit lands and converges.
        bob.run("text.insert", &json!({"text": "good "})).expect("Bob types");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "good ");
        bob.collab_tick();
        assert_eq!(bob.doc.plain_text(StoryRef::Body), "good ");
        assert_eq!(host.collab_stats()["unauthorized"], 1, "no further drops");
    }

    #[test]
    fn a_viewer_may_not_write_but_may_be_seen() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut viewer = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        viewer.collab_join("demo", "Vic", Box::new(tb));
        let table = || verified_table("aaaaaaaa", "bbbbbbbb", craft_collab::Role::Viewer, u64::MAX);
        host.collab_set_access(table());
        viewer.collab_set_access(table());
        viewer.collab_tick();
        host.collab_tick();
        viewer.collab_tick();

        viewer.run("text.insert", &json!({"text": "nope "})).expect("the viewer tries to type");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "", "a viewer's edit is dropped");
        assert_eq!(host.collab_stats()["unauthorized"], 1);

        viewer.collab_tick();
        host.collab_tick();
        let peers = host.collab_peers();
        assert_eq!(peers.len(), 1, "the viewer's cursor gets through");
    }

    #[test]
    fn a_snapshot_request_from_a_stranger_is_not_answered() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "cccccccc"]);
        let (ta, tm) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut mallory = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        host.collab_set_access(craft_collab::Access::new("demo", "aaaaaaaa", std::sync::Arc::new(TestVerifier)));
        mallory.collab_join("demo", "Mallory", Box::new(tm));
        mallory.collab_tick();
        host.collab_tick();
        mallory.collab_tick();
        assert_eq!(mallory.collab_stats()["joined"], false, "the stranger stays unjoined");
        assert!(host.collab_stats()["unauthorized"].as_u64().unwrap_or(0) >= 1, "the request was refused");
    }

    #[test]
    fn an_expired_grant_denies() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb"]);
        let (ta, tb) = (bus.remove(0), bus.remove(0));
        let mut host = session();
        let mut bob = session();
        host.collab_attach("demo", "Host", Box::new(ta));
        bob.collab_attach("demo", "Bob", Box::new(tb));
        let table = || verified_table("aaaaaaaa", "bbbbbbbb", craft_collab::Role::Editor, 1);
        host.collab_set_access(table());
        bob.collab_set_access(table());
        bob.run("text.insert", &json!({"text": "late "})).expect("Bob types");
        host.collab_tick();
        assert_eq!(host.doc.plain_text(StoryRef::Body), "", "an expired grant denies the edit");
        assert_eq!(host.collab_stats()["unauthorized"], 1);
    }

    #[test]
    fn only_replayable_commands_are_accepted() {
        assert!(replayable_command("format.italic"), "a plain editing command replays");
        assert!(replayable_command("text.newParagraph"));
        for id in ["file.open", "tools.recordMacro", "ui.click", "edit.undo", "text.insert", "format.bold", "collab.stats", "view.zoom"] {
            assert!(!replayable_command(id), "{id} must not replay");
        }
    }

    #[test]
    fn a_remote_command_leaves_no_local_undo() {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        drop(tb);
        let mut a = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        craft_collab::CollabHost::dispatch(&mut a, "text.insert", &json!({"text": "x"}), craft_collab::Origin::Remote { author: "bbbbbbbb".into() })
            .expect("a remote command dispatches");
        assert_eq!(a.doc.plain_text(StoryRef::Body), "x");
        assert!(!a.can_undo(), "a remote command leaves no local undo step");
    }
}
