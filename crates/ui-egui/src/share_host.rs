//! WordCraft's side of the shared Share and Sign in windows
//! (`macula-craft-share`): the session's shared document, the realm status
//! and the network hooks the app injected through [`Services`].

use serde_json::Value;
use wordcraft_engine::Session;

use crate::{Services, WordApp};

/// The windows' view of WordCraft for one frame.
struct WordShareHost<'a> {
    session: &'a mut Session,
    services: &'a Services,
    author: &'a str,
}

/// What a hook the build left out answers.
fn missing(what: &str) -> String {
    format!("this build has no {what} support")
}

impl craft_share::ShareHost for WordShareHost<'_> {
    fn collab(&self) -> Option<&craft_collab::CollabSession> {
        self.session.collab_session()
    }

    fn collab_mut(&mut self) -> Option<&mut craft_collab::CollabSession> {
        self.session.collab_session_mut()
    }

    fn realm_status(&self) -> Value {
        self.session.collab_realm()
    }

    fn default_name(&self) -> String {
        self.author.to_string()
    }

    fn hooks(&self) -> craft_share::Hooks {
        let s = self.services;
        craft_share::Hooks {
            join_invite: s.join_invite.is_some(),
            realm_refresh: s.realm_refresh.is_some(),
            sign_in: s.sign_in.is_some(),
            sign_in_check: s.sign_in_check.is_some(),
            identity_name: s.identity_name.is_some(),
            set_identity_name: s.set_identity_name.is_some(),
        }
    }

    fn join_invite(&mut self, invite: &str, name: &str) -> Result<(), String> {
        let join = self.services.join_invite.as_ref().ok_or_else(|| missing("mesh"))?;
        join(self.session, invite, name)
    }

    fn realm_refresh(&mut self) -> Result<Value, String> {
        let hook = self.services.realm_refresh.as_ref().ok_or_else(|| missing("realm"))?;
        hook(self.session)
    }

    fn sign_in(&mut self) -> Result<Value, String> {
        let hook = self.services.sign_in.as_ref().ok_or_else(|| missing("realm"))?;
        hook(self.session)
    }

    fn sign_in_check(&mut self) -> Result<Value, String> {
        let hook = self.services.sign_in_check.as_ref().ok_or_else(|| missing("realm"))?;
        hook(self.session)
    }

    fn identity_name(&mut self) -> Result<Value, String> {
        let hook = self.services.identity_name.as_ref().ok_or_else(|| missing("identity"))?;
        hook(self.session)
    }

    fn set_identity_name(&mut self, name: &str) -> Result<Value, String> {
        let hook = self.services.set_identity_name.as_ref().ok_or_else(|| missing("identity"))?;
        hook(self.session, name)
    }
}

/// Draws the Share and Sign in windows while they are open.
pub(crate) fn show(app: &mut WordApp, ctx: &egui::Context) {
    let mut host = WordShareHost { session: &mut app.session, services: &app.services, author: &app.ui.author };
    craft_share::share::show(ctx, &mut app.share, &mut app.login, &mut host);
    craft_share::login::show(ctx, &mut app.login, &mut host);
}
