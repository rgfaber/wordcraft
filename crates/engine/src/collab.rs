//! The collaboration glue: craft-collab's shared mechanism attached to a session.
//!
//! Wordcraft's adoption of the seam. Local commands are captured after they
//! apply as paragraph text updates (stable paragraph ids); peer updates are
//! applied through the same document model but outside the local undo
//! history; undo reverts only this author's own edits, as an inverse CRDT
//! edit, and shares that with the peers. The remote-command path
//! ([`run_with_origin`](crate::Session::run_with_origin)) exists for
//! mechanism adoption; the spike carries text and cursors only.

use serde_json::{Value, json};
use wordcraft_doc::{Document, Pos, StoryRef};

use crate::{CmdError, Session};

/// Everything a shared session needs: the peers' transport, the text CRDT
/// and the presence table.
pub struct CollabRuntime {
    doc: String,
    name: String,
    author: String,
    transport: Box<dyn craft_collab::Transport>,
    texts: craft_collab::TextMerge,
    presence: craft_collab::Presence,
    seq: u64,
    last_cursor: Option<String>,
    /// This session asked for the document snapshot and is waiting for it.
    awaiting_snapshot: bool,
    snapshot_requested: bool,
}

impl CollabRuntime {
    fn new(doc: &str, name: &str, transport: Box<dyn craft_collab::Transport>) -> CollabRuntime {
        let author = transport.author();
        CollabRuntime {
            doc: doc.to_string(),
            name: name.to_string(),
            author,
            transport,
            texts: craft_collab::TextMerge::new(),
            presence: craft_collab::Presence::default(),
            seq: 0,
            last_cursor: None,
            awaiting_snapshot: false,
            snapshot_requested: false,
        }
    }

    /// The messages for this document from other authors.
    fn poll(&mut self) -> Vec<craft_collab::Message> {
        let doc = self.doc.clone();
        let author = self.author.clone();
        self.transport.poll().into_iter().filter(|m| m.doc == doc && m.author != author).collect()
    }

    /// Queues a message for the peers.
    fn send(&mut self, kind: craft_collab::Kind) {
        self.seq += 1;
        let msg = craft_collab::Message { doc: self.doc.clone(), author: self.author.clone(), seq: self.seq, kind };
        let _ = self.transport.send(msg);
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

    /// Every paragraph's text by stable id: the pre-command image.
    pub(crate) fn collab_text_map(&self) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        let story = |s: StoryRef, doc: &Document, out: &mut Vec<(u64, String)>| {
            for path in doc.para_paths(s) {
                if let Some(p) = doc.para(s, &path)
                    && p.pid != 0
                {
                    out.push((p.pid, p.text.clone()));
                }
            }
        };
        story(StoryRef::Body, &self.doc, &mut out);
        let parts: Vec<u32> = self.doc.parts.keys().copied().collect();
        for id in parts {
            story(StoryRef::Part(id), &self.doc, &mut out);
        }
        out
    }

    /// Turns a successful local command's paragraph text changes into CRDT
    /// updates and queues them for the peers.
    pub(crate) fn collab_after(&mut self, before: &[(u64, String)]) {
        let after = self.collab_text_map();
        let mut updates = Vec::new();
        for (pid, text) in &after {
            let old = before.iter().find(|(p, _)| p == pid).map(|(_, t)| t.as_str()).unwrap_or("");
            if old == text {
                continue;
            }
            if let Some(c) = self.collab.as_mut()
                && let Some(update) = c.texts.local_edit(&pid.to_string(), old, text)
            {
                updates.push((pid.to_string(), update));
            }
        }
        for (para, update) in updates {
            if let Some(c) = self.collab.as_mut() {
                c.send(craft_collab::Kind::TextEdit { para, update });
            }
        }
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
            craft_collab::Kind::TextEdit { para, update } => {
                let merged = match self.collab.as_mut() {
                    Some(c) => c.texts.apply_update(&para, &update),
                    None => return,
                };
                let Ok(merged) = merged else { return };
                let Ok(pid) = para.parse::<u64>() else { return };
                if self.doc.set_paragraph_text_by_pid(pid, &merged) {
                    self.touch();
                    self.clamp_selection();
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
                if let Some(c) = self.collab.as_mut() {
                    c.send(craft_collab::Kind::Snapshot { data });
                }
            }
            craft_collab::Kind::Snapshot { data } => {
                let joining = matches!(&self.collab, Some(c) if c.awaiting_snapshot);
                if joining
                    && craft_collab::DocAdapter::load_snapshot(self, &data).is_ok()
                    && let Some(c) = self.collab.as_mut()
                {
                    c.awaiting_snapshot = false;
                }
            }
        }
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
            c.send(craft_collab::Kind::TextEdit { para: para.clone(), update });
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
