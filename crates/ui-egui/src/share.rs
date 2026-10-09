//! The Share window (spike): the host mints and copies an invite line,
//! approves the people waiting at the door and revokes holders; a joiner
//! pastes an invite and waits for its grant. The mesh wiring itself is
//! injected by the app (`Services::join_invite`); this window only drives
//! session state.

use egui::Context;

use crate::WordApp;

/// Everything the Share window remembers between frames.
#[derive(Default)]
pub struct ShareState {
    pub open: bool,
    /// The paste-in invite line and the display name of the join form.
    pub invite_input: String,
    pub name: String,
    /// The last invite this host minted.
    pub minted: Option<String>,
    /// The last action's outcome, shown under the buttons.
    pub status: Option<String>,
}

/// A node id shortened for display.
fn short(node: &str) -> String {
    node.chars().take(8).collect()
}

pub fn show(app: &mut WordApp, ctx: &Context) {
    if !app.share.open {
        return;
    }
    let mut open = app.share.open;
    egui::Window::new("Share")
        .open(&mut open)
        .collapsible(false)
        .default_width(460.0)
        .show(ctx, |ui| {
            if app.session.collab_active() {
                shared_panel(app, ui, ctx);
            } else {
                join_form(app, ui);
            }
            if let Some(msg) = app.share.status.clone() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(msg).small());
            }
        });
    app.share.open = open;
}

fn join_form(app: &mut WordApp, ui: &mut egui::Ui) {
    ui.label("Not sharing. Paste the invite line the host sent you:");
    ui.add(
        egui::TextEdit::multiline(&mut app.share.invite_input)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .hint_text("{\"v\":1,\"doc\":…}"),
    );
    ui.horizontal(|ui| {
        ui.label("Your name:");
        if app.share.name.is_empty() {
            app.share.name = if app.ui.author.trim().is_empty() { "Guest".into() } else { app.ui.author.clone() };
        }
        ui.add(egui::TextEdit::singleline(&mut app.share.name).desired_width(180.0));
    });
    let ready = !app.share.invite_input.trim().is_empty() && !app.share.name.trim().is_empty();
    ui.add_enabled_ui(ready, |ui| {
        if ui.button("Join").clicked() {
            let Some(join) = app.services.join_invite.as_ref() else {
                app.share.status = Some("This build has no mesh support.".into());
                return;
            };
            let outcome = join(&mut app.session, app.share.invite_input.trim(), app.share.name.trim());
            app.share.status = Some(match outcome {
                Ok(()) => "Joining: asking the host for a grant…".into(),
                Err(e) => format!("Join failed: {e}"),
            });
        }
    });
}

fn shared_panel(app: &mut WordApp, ui: &mut egui::Ui, ctx: &Context) {
    let stats = app.session.collab_stats();
    let doc = stats["doc"].as_str().unwrap_or("").to_string();
    let state = if stats["awaitingGrant"].as_bool().unwrap_or(false) {
        "waiting for the host's grant"
    } else if stats["joined"].as_bool().unwrap_or(false) {
        "joined"
    } else {
        "connecting"
    };
    ui.label(egui::RichText::new(format!("Sharing “{doc}” — {state}")).strong());

    if app.session.collab_can_invite() {
        ui.add_space(6.0);
        ui.label("Invite line (carries your signed token; valid 24 hours, editor):");
        ui.horizontal(|ui| {
            if ui.button("Mint and copy").clicked() {
                match app.session.collab_invite_from_context(craft_collab::Role::Editor, 24 * 60 * 60 * 1_000) {
                    Ok(line) => {
                        ctx.copy_text(line.clone());
                        app.share.minted = Some(line);
                        app.share.status = Some("Invite copied to the clipboard.".into());
                    }
                    Err(e) => app.share.status = Some(format!("Could not mint an invite: {e}")),
                }
            }
            if app.share.minted.is_some() && ui.button("Copy again").clicked()
                && let Some(line) = app.share.minted.clone()
            {
                ctx.copy_text(line);
                app.share.status = Some("Invite copied to the clipboard.".into());
            }
        });
        if let Some(line) = app.share.minted.clone() {
            let mut shown = line;
            ui.add(egui::TextEdit::multiline(&mut shown).desired_rows(2).desired_width(f32::INFINITY).interactive(false));
        }
    }

    let pending = app.session.collab_pending();
    if !pending.is_empty() {
        ui.add_space(6.0);
        ui.separator();
        ui.label("Waiting for your yes:");
        for (node, name) in pending {
            ui.horizontal(|ui| {
                ui.label(format!("{name} ({})", short(&node)));
                if ui.button("Approve").clicked() {
                    app.share.status = Some(match app.session.collab_approve(&node) {
                        Ok(()) => format!("Approved {name}."),
                        Err(e) => format!("Approve failed: {e}"),
                    });
                }
            });
        }
    }

    let holders = app.session.collab_holders();
    if !holders.is_empty() {
        ui.add_space(6.0);
        ui.separator();
        ui.label("People with access:");
        for holder in holders {
            ui.horizontal(|ui| {
                ui.label(short(&holder));
                if app.session.collab_can_invite() && ui.button("Revoke").clicked() {
                    app.share.status = Some(match app.session.collab_revoke(&holder) {
                        Ok(()) => format!("Revoked {}.", short(&holder)),
                        Err(e) => format!("Revoke failed: {e}"),
                    });
                }
            });
        }
    }
}
