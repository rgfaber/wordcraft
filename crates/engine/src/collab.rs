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
    /// This joiner is waiting for its grant.
    awaiting_grant: bool,
    grant_attempts: u32,
    last_grant_ms: u64,
    /// Join requests waiting for the host's yes: node id -> (name, at_ms).
    pending: std::collections::BTreeMap<String, (String, u64, craft_collab::Role)>,
    /// Whether the host admits a valid token without asking.
    auto_approve: bool,
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
            awaiting_grant: false,
            grant_attempts: 0,
            last_grant_ms: 0,
            pending: std::collections::BTreeMap::new(),
            auto_approve: false,
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
        self.collab = Some(CollabRuntime::new(doc, name, transport));
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

    /// Joins `doc` through an invite: presents its open token until the host
    /// grants this node, then asks for the snapshot.
    pub fn collab_join_via_invite(
        &mut self,
        doc: &str,
        name: &str,
        transport: Box<dyn craft_collab::Transport>,
        token: craft_collab::SignedGrant,
    ) {
        let mut runtime = CollabRuntime::new(doc, name, transport);
        runtime.token = Some(token);
        runtime.awaiting_grant = true;
        self.collab = Some(runtime);
    }

    /// Join requests awaiting the host's yes: (node id, display name).
    pub fn collab_pending(&self) -> Vec<(String, String)> {
        self.collab
            .as_ref()
            .map(|c| c.pending.iter().map(|(node, (name, _, _))| (node.clone(), name.clone())).collect())
            .unwrap_or_default()
    }

    /// The host approves a pending joiner: mints the grant from its token's
    /// role, broadcasts it and installs it.
    pub fn collab_approve(&mut self, node: &str) -> Result<(), String> {
        let role = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some((_, _, role)) = c.pending.get(node) else {
                return Err("no such pending joiner".into());
            };
            *role
        };
        if self.collab.as_ref().and_then(|c| c.issuer.as_ref()).is_none() {
            return Err("this session is not the host".into());
        }
        self.collab_issue_grant(node, role, now_ms());
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

    /// Mints this host's invite for the document: `realm` and `seed` are the
    /// mesh coordinates to share, verbatim. Returns the copyable line.
    pub fn collab_mint_invite(
        &mut self,
        realm: &str,
        seed: &str,
        role: craft_collab::Role,
        ttl_ms: u64,
    ) -> Result<String, String> {
        let now = now_ms();
        let (doc, host, host_key, host_name, token) = {
            let Some(c) = self.collab.as_ref() else { return Err("not collaborating".into()) };
            let Some(issuer) = c.issuer.as_ref() else { return Err("this session is not the host".into()) };
            let token = issuer.open_token(&c.doc, role, ttl_ms, now)?;
            (c.doc.clone(), issuer.node_id(), issuer.carried_key(), c.name.clone(), token)
        };
        let invite = craft_collab::Invite {
            v: craft_collab::INVITE_VERSION,
            doc,
            host,
            host_key: hex_bytes(&host_key),
            host_name,
            realm: realm.to_string(),
            seed: seed.to_string(),
            issued_ms: now,
            expires_ms: now.saturating_add(ttl_ms),
            token,
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
            && let Some(token) = c.token.clone()
        {
            c.grant_attempts += 1;
            c.last_grant_ms = now;
            c.send(craft_collab::Kind::JoinRequest { token, name: c.name.clone() });
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
        if let craft_collab::Kind::JoinRequest { token, name } = &msg.kind {
            self.collab_on_join_request(&msg.author, token, name);
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
                if self.doc.apply_paragraph_text(pid, &merged, carried.as_ref()) {
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
            }
            craft_collab::Kind::Revoked { revocation } => {
                let now = now_ms();
                if let Some(c) = self.collab.as_mut()
                    && let Some(access) = c.access.as_mut()
                {
                    let _ = access.revoke(&revocation, now);
                }
            }
            // Handled before the access check: it is the door itself.
            craft_collab::Kind::JoinRequest { .. } => {}
        }
    }

    /// The host judges a join request by its token: a valid open token gets
    /// a concrete grant (auto-approved when configured, else queued for the
    /// host's yes); an invalid one is refused and counted.
    fn collab_on_join_request(&mut self, author: &str, token: &craft_collab::SignedGrant, name: &str) {
        let now = now_ms();
        let role = {
            let Some(c) = self.collab.as_mut() else { return };
            if c.issuer.is_none() {
                return; // not the host: only the host admits
            }
            let Some(access) = c.access.as_ref() else { return };
            match access.check_token(token, now) {
                Ok(checked) => checked.role,
                Err(_) => return,
            }
        };
        let auto = matches!(&self.collab, Some(c) if c.auto_approve);
        if auto {
            self.collab_issue_grant(author, role, now);
        } else if let Some(c) = self.collab.as_mut() {
            c.pending.insert(author.to_string(), (name.to_string(), now, role));
        }
    }

    /// Mints a grant for `holder` as the host, broadcasts it and installs it
    /// locally.
    fn collab_issue_grant(&mut self, holder: &str, role: craft_collab::Role, now: u64) {
        let issued = {
            let Some(c) = self.collab.as_ref() else { return };
            let Some(issuer) = c.issuer.as_ref() else { return };
            match issuer.issue(&c.doc, holder, role, GRANT_TTL_MS, now) {
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
            "joined": !c.awaiting_snapshot,
            "seq": c.seq,
            "peers": peers,
            "latency": latency,
            "remoteErrors": c.remote_errors,
            "unauthorized": c.unauthorized,
            "access": c.access.is_some(),
            "awaitingGrant": c.awaiting_grant,
            "pending": c.pending.iter().map(|(node, (name, _, _))| json!({"node": short(node), "name": name})).collect::<Vec<_>>(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use craft_collab::MemoryTransport;
    use wordcraft_doc::StoryRef;

    fn session() -> Session {
        Session::new(Document::from_text(""))
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

        fn verify_revocation(
            &self,
            signed: &craft_collab::SignedRevocation,
            _now_ms: u64,
        ) -> Result<craft_collab::Revocation, String> {
            Ok(signed.revocation.clone())
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

        fn issue(
            &self,
            doc: &str,
            holder: &str,
            role: craft_collab::Role,
            ttl_ms: u64,
            now_ms: u64,
        ) -> Result<craft_collab::SignedGrant, String> {
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
    }

    fn host_table() -> craft_collab::Access {
        craft_collab::Access::new("demo", "aaaaaaaa", std::sync::Arc::new(TestVerifier))
    }

    fn token(host: &str, role: craft_collab::Role) -> craft_collab::SignedGrant {
        craft_collab::GrantIssuer::open_token(&TestIssuer { host: host.to_string() }, "demo", role, 60_000, now_ms())
            .expect("token")
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

        assert_eq!(host.collab_stats()["unauthorized"], 1, "the forged token is refused at the door");
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
            kind: craft_collab::Kind::Command {
                id: "text.newParagraph".into(),
                params: serde_json::to_vec(&json!({})).expect("json"),
                anchor: None,
            },
        })
        .expect("send");
        host.collab_tick();
        assert_eq!(host.collab_stats()["unauthorized"], 1, "the replayable command is not dropped");
        assert_eq!(host.doc.para_paths(StoryRef::Body).len(), 2, "and it applied: the paragraph split");
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
