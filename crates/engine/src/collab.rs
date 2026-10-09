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
    seq: u64,
    last_cursor: Option<String>,
    /// This session asked for the document snapshot and is waiting for it.
    awaiting_snapshot: bool,
    snapshot_requested: bool,
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
            seq: 0,
            last_cursor: None,
            awaiting_snapshot: false,
            snapshot_requested: false,
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
        let msg = craft_collab::Message { doc: self.doc.clone(), author: self.author.clone(), seq: self.seq, sent_at_ms: now_ms(), kind };
        let _ = self.transport.send(msg);
    }
}

/// Milliseconds since the epoch (0 where there is no clock).
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
    /// issued. `None` for commands that must not be shared (undo/redo, the
    /// spiked collab/debug/UI commands).
    pub(crate) fn collab_command_plan(&self, id: &str, params: &Value) -> Option<(String, Vec<u8>, Option<String>)> {
        if matches!(id, "edit.undo" | "edit.redo" | "edit.repeat" | "text.insert") {
            return None;
        }
        if id.starts_with("collab.")
            || id.starts_with("caret.")
            || id.starts_with("select.")
            || id.starts_with("view.")
            || id.starts_with("ui.")
            || id.starts_with("file.")
            || id.starts_with("tools.")
        {
            return None;
        }
        let bytes = serde_json::to_vec(params).ok()?;
        Some((id.to_string(), bytes, self.selection_json()))
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
        // A joiner asks for the shared document once.
        if let Some(c) = self.collab.as_mut()
            && c.awaiting_snapshot
            && !c.snapshot_requested
        {
            c.snapshot_requested = true;
            c.send(craft_collab::Kind::SnapshotRequest {});
        }
        self.collab_send_cursor();
    }

    fn collab_receive(&mut self, msg: craft_collab::Message) {
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
            }
            craft_collab::Kind::Command { id, params, anchor } => {
                self.collab_record_latency(&msg.author, msg.sent_at_ms);
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

        // B bolds "Hello"; A replays it, but must not keep B's selection.
        b.run("select.text", &json!({"text": "Hello"})).expect("B selects");
        b.run("format.bold", &json!({})).expect("B bolds");
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
