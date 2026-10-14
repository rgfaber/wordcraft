//! The collaboration glue: the seam's shared session attached to a WordCraft session.
//!
//! The protocol (join, grants, invites, contacts, snapshots, ordering,
//! presence, latency) is the seam's [`craft_collab::CollabSession`]; this
//! module is WordCraft's adoption of it. Local typing is captured as an exact
//! paragraph text update (stable pid, caret, text), paragraph structure as
//! leader splits and joins on the shared body text, and every other mutating
//! command as a [`Command`](craft_collab::Kind::Command) op with the selection
//! it ran under, which the peers replay through the same registry with
//! [`Origin::Remote`](craft_collab::Origin). Peer edits apply outside the
//! local undo history; undo reverts only this author's own edits.

use std::collections::HashMap;

use serde_json::{Value, json};
use wordcraft_doc::{Document, Pos};

#[cfg(test)]
use craft_collab::now_ms;

use crate::{CmdError, Session};

/// A shared session: the seam's protocol state plus what only WordCraft
/// keeps.
pub struct CollabRuntime {
    shared: craft_collab::CollabSession,
    /// Body paragraphs a join took out, by id: an undo (or a peer's) can
    /// bring one back, with its formatting.
    graveyard: HashMap<u64, wordcraft_doc::Paragraph>,
}

impl CollabRuntime {
    fn new(shared: craft_collab::CollabSession) -> CollabRuntime {
        CollabRuntime { shared, graveyard: HashMap::new() }
    }
}

/// A peer to draw: their name, colour and cursor position in this document.
pub struct PeerView {
    pub name: String,
    pub color: [u8; 3],
    pub pos: Option<Pos>,
}

/// One of the app's own collaboration files (grants, contacts). They are the
/// app's folders, not a command's, so automation roots don't apply
/// ([`crate::file_access::unconfined`]).
struct AppFile(String);

impl craft_collab::TextStore for AppFile {
    fn read(&self) -> Option<String> {
        crate::file_access::unconfined(|| crate::io::read_file(&self.0)).ok().and_then(|b| String::from_utf8(b).ok())
    }

    /// A failed write never takes the collaboration down.
    fn write(&self, text: &str) {
        if let Err(e) = crate::file_access::unconfined(|| crate::io::write_file(&self.0, text.as_bytes())) {
            log::warn!("collab: {e}");
        }
    }
}

impl Session {
    /// The document's shared session, while it is shared: what the share
    /// windows drive.
    pub fn collab_session(&self) -> Option<&craft_collab::CollabSession> {
        self.collab.as_ref().map(|c| &c.shared)
    }

    /// The same, mutable.
    pub fn collab_session_mut(&mut self) -> Option<&mut craft_collab::CollabSession> {
        self.collab.as_mut().map(|c| &mut c.shared)
    }

    /// Starts collaborating on `doc` as `name`, over `transport`: the body
    /// seeds the shared text (joiners receive it in the snapshot).
    pub fn collab_attach(&mut self, doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) {
        let body = body_paragraphs_of(&self.doc);
        self.collab = Some(CollabRuntime::new(craft_collab::CollabSession::host(doc, name, transport, &body)));
    }

    /// Joins an already-shared `doc` as `name`: the first tick asks a peer
    /// for the document snapshot, so stable paragraph ids line up before
    /// this author types.
    pub fn collab_join(&mut self, doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) {
        self.collab = Some(CollabRuntime::new(craft_collab::CollabSession::join(doc, name, transport)));
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
        self.collab = Some(CollabRuntime::new(craft_collab::CollabSession::join_requesting(doc, name, transport, token, membership)));
    }

    /// Whether this session is sharing its document.
    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    /// Turns on access control for this document: only the host and live
    /// grant holders act. Without it the session is open (spike mode).
    pub fn collab_set_access(&mut self, access: craft_collab::Access) {
        if let Some(c) = self.collab_session_mut() {
            c.set_access(access);
        }
    }

    /// Installs the host's signing side: with it, this session answers join
    /// requests with grants.
    pub fn collab_set_issuer(&mut self, issuer: Box<dyn craft_collab::GrantIssuer>) {
        if let Some(c) = self.collab_session_mut() {
            c.set_issuer(issuer);
        }
    }

    /// Whether the host admits a valid token without asking (the UI leaves
    /// this off and approves from `collab_pending`).
    pub fn collab_set_auto_approve(&mut self, on: bool) {
        if let Some(c) = self.collab_session_mut() {
            c.set_auto_approve(on);
        }
    }

    /// Remembers the mesh coordinates this host shares in, so the UI can
    /// mint an invite with one call.
    pub fn collab_set_share_context(&mut self, realm: &str, realm_key: &str, seed: &str, profile: &str) {
        if let Some(c) = self.collab_session_mut() {
            c.set_share_context(craft_collab::ShareContext {
                realm: realm.to_string(),
                realm_key: realm_key.to_string(),
                seed: seed.to_string(),
                profile: profile.to_string(),
            });
        }
    }

    /// The holders of this document (the UI's People list), sorted.
    pub fn collab_holders(&self) -> Vec<String> {
        self.collab_session().map(craft_collab::CollabSession::holders).unwrap_or_default()
    }

    /// Names the file where installed grants persist (the app's choice),
    /// and loads it: a restarted host relearns its holders and a restarted
    /// holder its own grant. Malformed or unverifiable entries are skipped;
    /// persistence never takes the collaboration down.
    pub fn collab_set_grants_path(&mut self, path: &str) {
        if let Some(c) = self.collab_session_mut() {
            c.set_grants_store(Box::new(AppFile(path.to_string())));
        }
    }

    /// Names the file where this person's contacts persist (the app's
    /// choice), and loads it: a restarted session keeps knowing the people
    /// it met. Malformed entries are skipped; persistence never takes the
    /// collaboration down.
    pub fn collab_set_contacts_path(&mut self, path: &str) {
        if let Some(c) = self.collab_session_mut() {
            c.set_contacts_store(Box::new(AppFile(path.to_string())));
        }
    }

    /// Remembers one sighting of a person: their node id, the name they gave
    /// (empty keeps what we have) and the realm, when known.
    pub fn collab_note_contact(&mut self, node: &str, name: &str, realm: Option<&str>) {
        if let Some(c) = self.collab_session_mut() {
            c.note_contact(node, name, realm);
        }
    }

    /// The people this device has met, most recently seen first.
    pub fn collab_contacts(&self) -> Vec<craft_collab::Contact> {
        self.collab_session().map(craft_collab::CollabSession::contacts).unwrap_or_default()
    }

    /// Forgets a person; true when one was known.
    pub fn collab_forget_contact(&mut self, node: &str) -> bool {
        self.collab_session_mut().is_some_and(|c| c.forget_contact(node))
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

    /// The host's own realm, when it is signed in to one: what an invite
    /// shows beside its name.
    fn host_realm(&self) -> Option<String> {
        self.realm.as_ref().filter(|r| r["signedIn"] == true).and_then(|r| r["realm"].as_str()).map(str::to_string)
    }

    /// Whether this session can mint invites (it is the host and knows its
    /// mesh coordinates).
    pub fn collab_can_invite(&self) -> bool {
        self.collab_session().is_some_and(craft_collab::CollabSession::can_invite)
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
        let host_realm = self.host_realm();
        let Some(c) = self.collab_session() else { return Err("not collaborating".into()) };
        c.mint_invite_for(None, host_realm.as_deref(), holder, invitee_name, role, ttl_ms)
    }

    /// Mints an invite from the remembered share context (the UI's Copy
    /// invite): `role` and `ttl_ms` as in `collab_mint_invite`.
    pub fn collab_invite_from_context(&mut self, role: craft_collab::Role, ttl_ms: u64) -> Result<String, String> {
        let host_realm = self.host_realm();
        let Some(c) = self.collab_session() else { return Err("not collaborating".into()) };
        c.mint_invite(None, host_realm.as_deref(), role, ttl_ms)
    }

    /// The host shares this document with a realm: members presenting a live
    /// membership UCAN of that realm are admitted with the verifier's role,
    /// and the grant issued on one is capped at the membership's own end.
    pub fn collab_set_member_verifier(&mut self, verifier: Box<dyn craft_collab::MembershipVerifier>) {
        if let Some(c) = self.collab_session_mut() {
            c.set_member_verifier(verifier);
        }
    }

    /// Whether this host shares with a realm.
    pub fn collab_shares_with_realm(&self) -> bool {
        self.collab_session().is_some_and(craft_collab::CollabSession::shares_with_realm)
    }

    /// Join requests awaiting the host's yes: (node id, display name).
    pub fn collab_pending(&self) -> Vec<(String, String)> {
        self.collab_session().map(craft_collab::CollabSession::pending).unwrap_or_default()
    }

    /// The host approves a pending joiner: mints the grant from its token's
    /// role (capped by the membership's end when the admission was one),
    /// broadcasts it and installs it.
    pub fn collab_approve(&mut self, node: &str) -> Result<(), String> {
        self.collab_session_mut().ok_or_else(|| "not collaborating".to_string())?.approve(node)
    }

    /// The host revokes a holder: signs it, broadcasts it, applies it.
    pub fn collab_revoke(&mut self, node: &str) -> Result<(), String> {
        self.collab_session_mut().ok_or_else(|| "not collaborating".to_string())?.revoke(node)
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
        let share = share_context(realm, realm_key, seed, profile);
        let host_realm = self.host_realm();
        let Some(c) = self.collab_session() else { return Err("not collaborating".into()) };
        c.mint_invite(Some(&share), host_realm.as_deref(), role, ttl_ms)
    }

    /// Mints a realm invite: no open token, and members of the host's realm
    /// are admitted with the verifier's role. The host must share with a
    /// realm (`collab_set_member_verifier`).
    pub fn collab_mint_realm_invite(&mut self, realm: &str, realm_key: &str, seed: &str, profile: &str, ttl_ms: u64) -> Result<String, String> {
        let share = share_context(realm, realm_key, seed, profile);
        let host_realm = self.host_realm();
        let Some(c) = self.collab_session() else { return Err("not collaborating".into()) };
        c.mint_realm_invite(Some(&share), host_realm.as_deref(), ttl_ms)
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
        let share = share_context(realm, realm_key, seed, profile);
        let host_realm = self.host_realm();
        let Some(c) = self.collab_session() else { return Err("not collaborating".into()) };
        c.mint_invite_for(Some(&share), host_realm.as_deref(), holder, invitee_name, role, ttl_ms)
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
            if let Some(u) = c.shared.texts_mut().local_edit_exact(&para, index, remove, &insert) {
                updates.push((u, props.clone()));
            }
            if let Some(u) = c.shared.texts_mut().sync(&para, &doc_text) {
                updates.push((u, None));
            }
        }
        for (update, props) in updates {
            if let Some(c) = self.collab.as_mut() {
                c.shared.send_text(para.clone(), update, props);
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
            Some(c) => c.shared.texts_mut().local_bold(&para, index, len, bold),
            None => return,
        };
        if let Some(update) = update
            && let Some(c) = self.collab.as_mut()
        {
            c.shared.send_text(para.clone(), update, None);
        }
        self.collab_sync_bold(pid);
    }

    /// Aligns a paragraph's bold to the CRDT's item-anchored spans: the
    /// derivation every replica shares, so runs cover the same characters.
    pub(crate) fn collab_sync_bold(&mut self, pid: u64) {
        let spans = match self.collab.as_ref() {
            Some(c) => c.shared.texts().bold_spans(&pid.to_string()),
            None => return,
        };
        if self.doc.set_paragraph_bold_by_pid(pid, &spans) {
            self.touch();
        }
    }

    /// Queues a captured command for the peers.
    pub(crate) fn collab_send_command(&mut self, id: String, params: Vec<u8>, anchor: Option<String>) {
        if let Some(c) = self.collab_session_mut() {
            c.send_command(id, params, anchor);
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
        let events = match self.collab_session_mut() {
            Some(c) => c.tick(),
            None => return,
        };
        for event in events {
            self.collab_apply(event);
        }
        let cursor = self.cursor_json();
        if let Some(c) = self.collab_session_mut() {
            c.share_cursor(cursor);
        }
    }

    /// Applies one peer event the seam let through its door.
    fn collab_apply(&mut self, event: craft_collab::Inbound) {
        match event {
            craft_collab::Inbound::Text { para, update, props, .. } => {
                let carried_props: Option<wordcraft_doc::CharProps> = props.as_deref().and_then(|b| serde_json::from_slice(b).ok());
                self.collab_apply_remote_text(&para, &update, carried_props.as_ref());
            }
            craft_collab::Inbound::Command { author, id, params, anchor } => {
                // A holder may replay only the commands the local capture
                // would ever send; anything else (file, tools, UI, undo) is
                // refused on receive, whoever the author is.
                if !replayable_command(&id) {
                    if let Some(c) = self.collab_session_mut() {
                        c.note_refused();
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
                let result = self.run_with_origin(&id, &parsed, craft_collab::Origin::Remote { author });
                self.sel = previous;
                self.clamp_selection();
                if let Err(e) = result {
                    if let Some(c) = self.collab_session_mut() {
                        c.note_remote_error();
                    }
                    self.status = format!("remote {id}: {e}");
                }
            }
            craft_collab::Inbound::SnapshotWanted => {
                let data = craft_collab::DocAdapter::snapshot(self);
                if let Some(c) = self.collab_session_mut() {
                    c.answer_snapshot(data);
                }
            }
            craft_collab::Inbound::Snapshot { data, crdt } => {
                let joining = self.collab_session().is_some_and(craft_collab::CollabSession::awaiting_snapshot);
                if joining
                    && craft_collab::DocAdapter::load_snapshot(self, &data).is_ok()
                    && let Some(c) = self.collab_session_mut()
                {
                    // Seed the CRDT too, or post-join merges would not align
                    // with the peers' item history.
                    c.snapshot_loaded(&crdt);
                }
            }
        }
    }

    /// The collaboration status for agents: peers, join state, per-pair
    /// latency (p50/p95) and the count of remote commands that failed to
    /// apply (a divergence signal).
    pub fn collab_stats(&self) -> Value {
        self.collab_session().map(craft_collab::CollabSession::stats).unwrap_or_else(|| json!({"active": false}))
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
        if self.collab_session_mut().and_then(craft_collab::CollabSession::undo_local).is_none() {
            return false;
        }
        // The undo may have touched any paragraph (a split, a join, typing
        // anywhere): the document follows the shared text.
        self.collab_reconcile_body(None, None);
        self.collab_reconcile_outside(None);
        self.clamp_selection();
        true
    }

    /// The peers to draw, their cursors resolved to positions where known.
    pub fn collab_peers(&self) -> Vec<PeerView> {
        let Some(c) = self.collab_session() else { return Vec::new() };
        c.peers()
            .map(|p| PeerView { name: p.name.clone(), color: p.color, pos: p.position.as_deref().and_then(|s| cursor_pos(&self.doc, s)) })
            .collect()
    }
}

/// The mesh coordinates an invite carries, verbatim.
fn share_context(realm: &str, realm_key: &str, seed: &str, profile: &str) -> craft_collab::ShareContext {
    craft_collab::ShareContext { realm: realm.to_string(), realm_key: realm_key.to_string(), seed: seed.to_string(), profile: profile.to_string() }
}

/// The document's top-level body paragraphs as the shared text holds them:
/// id, text and paragraph properties.
fn body_paragraphs_of(doc: &Document) -> Vec<craft_collab::BodyParagraph> {
    doc.body
        .iter()
        .filter_map(|b| match &**b {
            wordcraft_doc::Block::Para(p) => {
                Some(craft_collab::BodyParagraph { para: p.pid.to_string(), text: p.text.clone(), props: serde_json::to_string(&p.props).ok() })
            }
            _ => None,
        })
        .collect()
}

/// The top-level body blocks that are not paragraphs (tables and the like),
/// in order: a structural capture only covers paragraphs, so a command that
/// changes these keeps the replay path.
fn other_blocks_of(doc: &Document) -> Vec<Arc<wordcraft_doc::Block>> {
    doc.body.iter().filter(|b| !matches!(&***b, wordcraft_doc::Block::Para(_))).cloned().collect()
}

use std::sync::Arc;

impl Session {
    /// Where a position is, as the shared text names it: a top-level body
    /// paragraph's id and the offset in it. `None` elsewhere (a table cell).
    fn body_position(&self, pos: &Pos) -> Option<(String, u32)> {
        if pos.story != wordcraft_doc::StoryRef::Body || pos.path.0.len() != 1 {
            return None;
        }
        let para = self.doc.para_at(pos)?;
        Some((para.pid.to_string(), pos.off as u32))
    }

    /// A peer's text update: applied with this author's caret carried across
    /// it, then the document follows the shared text.
    fn collab_apply_remote_text(&mut self, para: &str, update: &[u8], props: Option<&wordcraft_doc::CharProps>) {
        let ends = [self.body_position(&self.sel.anchor), self.body_position(&self.sel.focus)];
        let positions: Vec<(String, u32)> = ends.iter().flatten().cloned().collect();
        let carried = match self.collab.as_mut() {
            Some(c) => c.shared.texts_mut().apply_update_carrying(update, &positions),
            None => return,
        };
        let Ok(carried) = carried else { return };
        // Outside the body only when this document holds that paragraph in
        // a table: a split's new paragraph or a join's vanished one is not
        // in the body text by name, yet it is a body change.
        let outside = para.parse::<u64>().ok().and_then(|pid| self.doc.paragraph_by_pid(pid)).is_some_and(|(_, path)| path.0.len() > 1);
        let in_body = !outside;
        if in_body {
            self.collab_reconcile_body(Some(para), props);
            // The caret's ends where the shared text carried them.
            let mut carried = carried.into_iter();
            for (end, was) in [&mut self.sel.anchor, &mut self.sel.focus].into_iter().zip(ends) {
                if was.is_some()
                    && let Some((pid, off)) = carried.next()
                    && let Ok(pid) = pid.parse::<u64>()
                    && let Some((story, path)) = self.doc.paragraph_by_pid(pid)
                {
                    *end = Pos { story, path, off: off as usize };
                }
            }
        } else {
            self.collab_reconcile_outside(Some(para));
        }
        self.clamp_selection();
    }

    /// Makes the document's top-level body paragraphs what the shared text
    /// says: their order, which exist, and their text. A remote split takes
    /// its tail (formatting included) from the paragraph it split; a remote
    /// join appends the joined paragraph; anything else is set from the text.
    /// `props_for` names the paragraph a peer's typed text went into, with
    /// the formatting it carried.
    pub(crate) fn collab_reconcile_body(&mut self, props_for: Option<&str>, props: Option<&wordcraft_doc::CharProps>) {
        let paras = match self.collab.as_ref() {
            Some(c) => c.shared.texts().body_paragraphs(),
            None => return,
        };
        if paras.is_empty() {
            return;
        }
        let wanted: Vec<u64> = paras.iter().filter_map(|p| p.para.parse().ok()).collect();
        let wanted_set: std::collections::HashSet<u64> = wanted.iter().copied().collect();
        // The old body: paragraphs by id, in order, and the other blocks
        // each paragraph is followed by.
        let mut old_paras: HashMap<u64, wordcraft_doc::Paragraph> = HashMap::new();
        let mut old_order: Vec<u64> = Vec::new();
        let mut leading: Vec<Arc<wordcraft_doc::Block>> = Vec::new();
        let mut trailing: HashMap<u64, Vec<Arc<wordcraft_doc::Block>>> = HashMap::new();
        let mut last_kept: Option<u64> = None;
        for b in &self.doc.body {
            match &**b {
                wordcraft_doc::Block::Para(p) => {
                    old_paras.insert(p.pid, p.clone());
                    old_order.push(p.pid);
                    if wanted_set.contains(&p.pid) {
                        last_kept = Some(p.pid);
                    }
                }
                _ => match last_kept {
                    Some(pid) => trailing.entry(pid).or_default().push(b.clone()),
                    None => leading.push(b.clone()),
                },
            }
        }
        let shared: HashMap<u64, &craft_collab::BodyParagraph> = paras.iter().filter_map(|p| p.para.parse::<u64>().ok().map(|id| (id, p))).collect();
        let mut blocks = leading;
        let mut prev: Option<u64> = None;
        for pid in &wanted {
            let target = shared.get(pid).map(|p| p.text.as_str()).unwrap_or("");
            let para = match old_paras.get(pid).cloned() {
                Some(mut p) => {
                    // A join: the removed paragraphs right after it in the old
                    // order, appended, make the shared text.
                    let at = old_order.iter().position(|x| x == pid).unwrap_or(0);
                    let joined: Vec<u64> = old_order[at + 1..].iter().copied().take_while(|x| !wanted_set.contains(x)).collect();
                    let joined_text: String = joined.iter().filter_map(|x| old_paras.get(x)).map(|q| q.text.as_str()).collect();
                    if !joined.is_empty() && format!("{}{}", p.text, joined_text) == target {
                        for x in &joined {
                            if let Some(q) = old_paras.get(x) {
                                p.append(q.clone());
                            }
                        }
                    } else if target.len() < p.text.len() && p.text.starts_with(target) {
                        // A split: the tail went to the next paragraph.
                        let _ = p.split_off(target.len());
                    }
                    p
                }
                None => {
                    // New here: a peer's split takes its tail from the paragraph
                    // it split, formatting included; a paragraph an undo brings
                    // back comes out of the graveyard; anything else is new.
                    let from_split = prev.and_then(|q| old_paras.get(&q)).and_then(|q| {
                        let head = shared.get(&q.pid).map(|s| s.text.as_str()).unwrap_or("");
                        (q.text.len() >= head.len() && q.text.starts_with(head) && &q.text[head.len()..] == target)
                            .then(|| q.clone().split_off(head.len()).ok())
                            .flatten()
                    });
                    let restored = self.collab.as_mut().and_then(|c| c.graveyard.remove(pid));
                    let mut p = from_split.or(restored).unwrap_or_default();
                    if let Some(props) = shared.get(pid).and_then(|s| s.props.as_deref()).and_then(|j| serde_json::from_str(j).ok()) {
                        p.props = props;
                    }
                    p
                }
            };
            let mut para = para;
            para.pid = *pid;
            blocks.push(Arc::new(wordcraft_doc::Block::Para(para)));
            if let Some(after) = trailing.remove(pid) {
                blocks.extend(after);
            }
            prev = Some(*pid);
        }
        // Paragraphs the shared text no longer holds go to the graveyard.
        if let Some(c) = self.collab.as_mut() {
            for pid in old_order.iter().filter(|x| !wanted_set.contains(x)) {
                if let Some(p) = old_paras.get(pid) {
                    c.graveyard.insert(*pid, p.clone());
                }
            }
        }
        let changed = blocks.len() != self.doc.body.len() || blocks.iter().zip(&self.doc.body).any(|(a, b)| a != b);
        if changed {
            self.doc.body = blocks;
            self.doc.ensure_nonempty();
            self.touch();
        }
        // Then every paragraph's text is the shared text's.
        for p in &paras {
            let Ok(pid) = p.para.parse::<u64>() else { continue };
            let carried = if props_for == Some(p.para.as_str()) { props } else { None };
            if self.doc.splice_paragraph_text(pid, &p.text, carried).is_some_and(|(_, _, _, r, i)| r + i > 0) {
                self.touch();
            }
            self.collab_sync_bold(pid);
        }
    }

    /// Paragraphs outside the body (table cells): each follows its own
    /// shared text. `only` names the one a peer's edit touched; `None` checks
    /// them all (after an undo).
    pub(crate) fn collab_reconcile_outside(&mut self, only: Option<&str>) {
        let pids: Vec<u64> = match only {
            Some(para) => para.parse().into_iter().collect(),
            None => self
                .doc
                .para_paths(wordcraft_doc::StoryRef::Body)
                .iter()
                .filter(|p| p.0.len() > 1)
                .filter_map(|p| self.doc.para(wordcraft_doc::StoryRef::Body, p).map(|q| q.pid))
                .collect(),
        };
        for pid in pids {
            let Some(c) = self.collab.as_ref() else { return };
            if c.shared.texts().in_body(&pid.to_string()) {
                continue;
            }
            let text = c.shared.texts().text(&pid.to_string());
            let current = self.doc.paragraph_by_pid(pid).and_then(|(s, p)| self.doc.para(s, &p).map(|q| q.text.clone()));
            if only.is_none() && text.is_empty() && current.as_deref().is_some_and(|t| !t.is_empty()) {
                continue; // never shared: nothing to follow
            }
            if let Some((story, path, pre, removed, inserted)) = self.doc.splice_paragraph_text(pid, &text, None) {
                for pos in [&mut self.sel.anchor, &mut self.sel.focus] {
                    if pos.story == story && pos.path == path {
                        pos.off = shifted_off(pos.off, pre, removed, inserted);
                    }
                }
                if removed + inserted > 0 {
                    self.touch();
                }
            }
            self.collab_sync_bold(pid);
        }
    }

    /// A local command that added or removed top-level body paragraphs
    /// (Enter, Backspace or Delete at a boundary, a multi-paragraph paste):
    /// shared as leader inserts and deletes plus text syncs, never as a
    /// replayed command, so every replica keeps the same paragraph ids. The
    /// local document then follows the shared text too. False when the
    /// paragraph structure did not change, or other blocks changed with it
    /// (those keep the replay path).
    pub(crate) fn collab_capture_structure(&mut self, before: &Document) -> bool {
        let old = body_paragraphs_of(before);
        let new = body_paragraphs_of(&self.doc);
        let old_ids: Vec<&str> = old.iter().map(|p| p.para.as_str()).collect();
        let new_ids: Vec<&str> = new.iter().map(|p| p.para.as_str()).collect();
        if old_ids == new_ids || other_blocks_of(before) != other_blocks_of(&self.doc) {
            return false;
        }
        let Some(c) = self.collab.as_mut() else { return false };
        let mut updates: Vec<(String, Vec<u8>)> = Vec::new();
        // Joins first, last to first, so each leader is still where it was.
        for id in old_ids.iter().rev().filter(|id| !new_ids.contains(id)) {
            if let Some(u) = c.shared.texts_mut().local_join(id) {
                updates.push((id.to_string(), u));
            }
        }
        // Splits in document order: each new paragraph opens after the one
        // before it, at where that one's new text ends.
        for (i, p) in new.iter().enumerate() {
            if old_ids.contains(&p.para.as_str()) {
                continue;
            }
            let Some(prev) = i.checked_sub(1).map(|j| &new[j]) else { continue };
            let at = prev.text.len().min(c.shared.texts_mut().text(&prev.para).len()) as u32;
            if let Some(u) = c.shared.texts_mut().local_split(&prev.para, at, &p.para, p.props.as_deref()) {
                updates.push((p.para.clone(), u));
            }
        }
        // Then every paragraph's text is the document's.
        for p in &new {
            if let Some(u) = c.shared.texts_mut().sync(&p.para, &p.text) {
                updates.push((p.para.clone(), u));
            }
        }
        for (para, update) in updates {
            c.shared.send_text(para, update, None);
        }
        self.collab_reconcile_body(None, None);
        true
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

    /// The demo's tidy layout, checked before it is recorded: a heading-styled
    /// title, then each author's own paragraph (Enter, then typing), then
    /// each edits their own paragraph again. Every replica must agree on the
    /// paragraphs, their text and their style, and later edits must land in
    /// the right paragraph everywhere (the paragraph ids must match).
    #[test]
    fn a_heading_and_each_authors_own_paragraph_travel() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb", "cccccccc"]);
        let (ta, tb, tc) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut h = session();
        let mut i = session();
        let mut e = session();
        h.collab_attach("demo", "Hana", Box::new(ta));
        i.collab_join("demo", "Ines", Box::new(tb));
        e.collab_join("demo", "Eve", Box::new(tc));
        let tick = |h: &mut Session, i: &mut Session, e: &mut Session| {
            for _ in 0..3 {
                i.collab_tick();
                e.collab_tick();
                h.collab_tick();
            }
        };
        tick(&mut h, &mut i, &mut e);
        h.run("text.insert", &json!({"text": "Field notes"})).expect("title");
        h.run("para.heading1", &json!({})).expect("heading");
        h.run("text.newParagraph", &json!({})).expect("enter");
        h.run("text.insert", &json!({"text": "Hana, via helsinki."})).expect("Hana's paragraph");
        tick(&mut h, &mut i, &mut e);
        for (who, s) in [("Ines, via nuremberg.", &mut i), ("Eve, via amsterdam.", &mut e)] {
            let last = s.doc.para_paths(StoryRef::Body).len() - 1;
            let len = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(last)).map(|p| p.len()).unwrap_or(0);
            s.run("caret.set", &json!({"pos": {"story": "body", "path": [last], "off": len}})).expect("caret at the end");
            s.run("text.newParagraph", &json!({})).expect("enter");
            s.run("text.insert", &json!({"text": who})).expect("own paragraph");
        }
        tick(&mut h, &mut i, &mut e);
        // Each edits their own paragraph again.
        i.run("text.insert", &json!({"text": " More from Ines."})).expect("Ines again");
        e.run("text.insert", &json!({"text": " More from Eve."})).expect("Eve again");
        h.run("caret.set", &json!({"pos": {"story": "body", "path": [1], "off": 19}})).expect("Hana's end");
        h.run("text.insert", &json!({"text": " More from Hana."})).expect("Hana again");
        tick(&mut h, &mut i, &mut e);

        let shape = |s: &Session| -> Vec<(String, Option<String>)> {
            (0..s.doc.para_paths(StoryRef::Body).len())
                .map(|n| {
                    let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(n)).expect("para");
                    (p.text.clone(), p.props.style.clone())
                })
                .collect()
        };
        let hs = shape(&h);
        assert_eq!(hs, shape(&i), "Hana and Ines agree");
        assert_eq!(hs, shape(&e), "Hana and Eve agree");
        assert_eq!(hs.len(), 4, "title + three paragraphs: {hs:?}");
        assert_eq!(hs[0].1.as_deref(), Some("Heading1"), "the title is a heading everywhere: {hs:?}");
        let texts: Vec<&str> = hs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(texts.contains(&"Hana, via helsinki. More from Hana."), "{texts:?}");
        assert!(texts.contains(&"Ines, via nuremberg. More from Ines."), "{texts:?}");
        assert!(texts.contains(&"Eve, via amsterdam. More from Eve."), "{texts:?}");
    }

    fn two_authors() -> (Session, Session) {
        let (ta, tb) = MemoryTransport::pair("aaaaaaaa", "bbbbbbbb");
        let mut a = session();
        let mut b = session();
        a.collab_attach("demo", "Alice", Box::new(ta));
        b.collab_join("demo", "Bob", Box::new(tb));
        b.collab_tick();
        a.collab_tick();
        b.collab_tick();
        (a, b)
    }

    fn settle(a: &mut Session, b: &mut Session) {
        for _ in 0..3 {
            a.collab_tick();
            b.collab_tick();
        }
    }

    fn paras(s: &Session) -> Vec<String> {
        (0..s.doc.para_paths(StoryRef::Body).len())
            .map(|n| s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(n)).map(|p| p.text.clone()).unwrap_or_default())
            .collect()
    }

    fn caret(s: &mut Session, path: usize, off: usize) {
        s.run("caret.set", &json!({"pos": {"story": "body", "path": [path], "off": off}})).expect("caret");
    }

    #[test]
    fn a_backspace_join_travels_and_typing_after_it_lands_everywhere() {
        let (mut a, mut b) = two_authors();
        a.run("text.insert", &json!({"text": "one"})).expect("type");
        a.run("text.newParagraph", &json!({})).expect("enter");
        a.run("text.insert", &json!({"text": "two"})).expect("type");
        settle(&mut a, &mut b);
        // AutoCorrect capitalizes the first word on A; the corrected text is what travels.
        assert_eq!(paras(&a), vec!["One", "two"]);
        assert_eq!(paras(&b), paras(&a));
        caret(&mut b, 1, 0);
        b.run("text.backspace", &json!({})).expect("backspace at the paragraph start");
        settle(&mut a, &mut b);

        assert_eq!(paras(&a), vec!["Onetwo"], "the join reached A");
        b.run("text.insert", &json!({"text": " and three"})).expect("B types on");
        settle(&mut a, &mut b);
        assert_eq!(paras(&a), paras(&b));
        assert!(paras(&a)[0].contains("and three"), "{:?}", paras(&a));
    }

    #[test]
    fn two_concurrent_enters_in_one_paragraph_converge_without_a_duplicate() {
        let (mut a, mut b) = two_authors();
        a.run("text.insert", &json!({"text": "aaaa bbbb cccc"})).expect("type");
        settle(&mut a, &mut b);
        caret(&mut a, 0, 5);
        caret(&mut b, 0, 10);
        a.run("text.newParagraph", &json!({})).expect("A enter");
        b.run("text.newParagraph", &json!({})).expect("B enter");
        settle(&mut a, &mut b);
        assert_eq!(paras(&a), paras(&b), "converged");
        assert_eq!(paras(&a).concat(), "aaaa bbbb cccc", "nothing duplicated or lost: {:?}", paras(&a));
        assert_eq!(paras(&a).len(), 3);
    }

    #[test]
    fn undoing_an_enter_rejoins_the_paragraph_for_everyone() {
        let (mut a, mut b) = two_authors();
        a.run("text.insert", &json!({"text": "left right"})).expect("type");
        settle(&mut a, &mut b);
        caret(&mut a, 0, 5);
        a.run("text.newParagraph", &json!({})).expect("enter");
        settle(&mut a, &mut b);
        assert_eq!(paras(&b), vec!["left ", "right"]);
        assert!(a.undo(), "A undoes its Enter");
        settle(&mut a, &mut b);
        assert_eq!(paras(&a), vec!["left right"]);
        assert_eq!(paras(&b), vec!["left right"], "rejoined at B too");
    }

    #[test]
    fn a_peer_split_carries_the_local_caret_into_the_new_paragraph() {
        let (mut a, mut b) = two_authors();
        a.run("text.insert", &json!({"text": "Field notes. Hana writes."})).expect("type");
        settle(&mut a, &mut b);
        caret(&mut b, 0, 25); // B at the end
        caret(&mut a, 0, 13);
        a.run("text.newParagraph", &json!({})).expect("A splits before Hana");
        settle(&mut a, &mut b);
        b.run("text.insert", &json!({"text": " More."})).expect("B types on");
        settle(&mut a, &mut b);
        assert_eq!(paras(&a), vec!["Field notes. ", "Hana writes. More."]);
        assert_eq!(paras(&a), paras(&b));
    }

    #[test]
    fn a_split_keeps_the_tails_formatting_on_the_peer() {
        let (mut a, mut b) = two_authors();
        a.run("text.insert", &json!({"text": "plain italic"})).expect("type");
        a.run("select.text", &json!({"text": "italic"})).expect("select");
        a.run("format.italic", &json!({})).expect("italic");
        settle(&mut a, &mut b);
        caret(&mut a, 0, 6);
        a.run("text.newParagraph", &json!({})).expect("enter before italic");
        settle(&mut a, &mut b);
        assert_eq!(paras(&b), vec!["plain ", "italic"]);
        let p = b.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(1)).expect("tail");
        assert_eq!(p.props_of_char(0).italic, Some(true), "the tail kept its italic at B");
        assert_eq!(runs_of(&a), runs_of(&b));
    }

    /// Today's working layout: paragraphs the host makes BEFORE the others
    /// join reach them in the snapshot, ids included, so each author can
    /// write in their own paragraph and every replica agrees.
    #[test]
    fn paragraphs_made_before_the_join_carry_their_ids_to_every_author() {
        let mut bus = MemoryTransport::bus(&["aaaaaaaa", "bbbbbbbb", "cccccccc"]);
        let (ta, tb, tc) = (bus.remove(0), bus.remove(0), bus.remove(0));
        let mut h = session();
        let mut i = session();
        let mut e = session();
        h.collab_attach("demo", "Hana", Box::new(ta));
        h.run("text.insert", &json!({"text": "Field notes"})).expect("title");
        h.run("para.heading1", &json!({})).expect("heading");
        for _ in 0..3 {
            h.run("text.newParagraph", &json!({})).expect("enter");
        }
        h.run("para.normal", &json!({})).ok();
        i.collab_join("demo", "Ines", Box::new(tb));
        e.collab_join("demo", "Eve", Box::new(tc));
        let tick = |h: &mut Session, i: &mut Session, e: &mut Session| {
            for _ in 0..3 {
                i.collab_tick();
                e.collab_tick();
                h.collab_tick();
            }
        };
        tick(&mut h, &mut i, &mut e);
        for (who, n, s) in [("Hana, via helsinki.", 1, &mut h), ("Ines, via nuremberg.", 2, &mut i), ("Eve, via amsterdam.", 3, &mut e)] {
            s.run("caret.set", &json!({"pos": {"story": "body", "path": [n], "off": 0}})).expect("caret");
            s.run("text.insert", &json!({"text": who})).expect("own paragraph");
        }
        tick(&mut h, &mut i, &mut e);
        let shape = |s: &Session| -> Vec<(String, Option<String>)> {
            (0..s.doc.para_paths(StoryRef::Body).len())
                .map(|n| {
                    let p = s.doc.para(StoryRef::Body, &wordcraft_doc::Path::top(n)).expect("para");
                    (p.text.clone(), p.props.style.clone())
                })
                .collect()
        };
        let hs = shape(&h);
        assert_eq!(hs, shape(&i));
        assert_eq!(hs, shape(&e));
        assert_eq!(hs[0], ("Field notes".to_string(), Some("Heading1".to_string())));
        assert_eq!(hs[1].0, "Hana, via helsinki.");
        assert_eq!(hs[2].0, "Ines, via nuremberg.");
        assert_eq!(hs[3].0, "Eve, via amsterdam.");
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
