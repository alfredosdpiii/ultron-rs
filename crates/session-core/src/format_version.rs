//! The resume-time format check (format-version.ts `assertSessionFormatsReadable`): every versioned session value
//! Ultron owns must be readable by this build. The first value written in a format this build does not know fails
//! the open, naming the value and its version, before any of it is loaded, settled or rewritten.

use session_journal::Session;
use session_journal::ns;

/// `assertSessionFormatsReadable`: the error of the first value in an unknown format version. Artifacts are checked
/// when read.
pub fn assert_session_formats_readable(session: &Session) -> Result<(), String> {
    for document in ns::VERSIONED {
        kernel_apis::store::read_versioned(
            &document.address(),
            session.value(document.namespace, document.key).cloned(),
            document.version_field,
        )?;
    }
    for stored in session.state().scan_values(ns::MODULE, "") {
        kernel_apis::store::read_versioned(
            &format!("{}/{}", ns::MODULE, stored.key),
            Some(stored.value.clone()),
            ns::module_version_field(&stored.key),
        )?;
    }
    Ok(())
}
