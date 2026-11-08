//! [`Op::ContinueFrom`]: a new session continues an old one by searching its log (crate::continuation).

use super::*;

impl Core {
    pub(super) fn continue_from(&mut self, id: &str) -> Result<Value, String> {
        {
            let session = lock_session(&self.session);
            if session.id() == id {
                return Err("A session cannot continue itself".into());
            }
            if session
                .branch_entry_refs(MAIN_LANE)?
                .iter()
                .any(|entry| entry["type"] == "message")
            {
                return Err("Only a new session can continue another: this one already has messages".into());
            }
        }
        let roots = crate::compose::session_roots(&self.runtime.config, &self.runtime.sessions_dir);
        let old = session_journal::search::find(&roots, id)?;
        let name = session_journal::names::listing(old.clone()).name;
        let note = crate::continuation::message(&old, name.as_deref(), jsrt::date::now_ms());
        lock_session(&self.session).append_message(MAIN_LANE, note)?;
        Ok(json!({"session": old.id, "name": name}))
    }
}
