//! A server directory's socket file names: `<serverId>.sock` (the server's address clients connect to),
//! `control-<serverId>.sock` (its coordinator's control socket) and `server-<serverId>-<nonce>.sock` (one
//! generation's endpoint).

use crate::message::is_server_id;

const SUFFIX: &str = ".sock";
const GENERATION_PREFIX: &str = "server-";
/// The hex digits of a generation endpoint's nonce.
pub const NONCE_LEN: usize = 12;
/// The longest name a server directory holds: a generation endpoint's.
pub const LONGEST_NAME: usize = GENERATION_PREFIX.len() + 36 + 1 + NONCE_LEN + SUFFIX.len();

pub fn endpoint_name(server_id: &str) -> String {
    format!("{server_id}{SUFFIX}")
}

pub fn control_name(server_id: &str) -> String {
    format!("control-{server_id}{SUFFIX}")
}

pub fn generation_name(server_id: &str, nonce: &str) -> String {
    format!("{GENERATION_PREFIX}{server_id}-{nonce}{SUFFIX}")
}

/// Whether `file_name` is a generation endpoint of `server_id`.
pub fn is_generation_of(file_name: &str, server_id: &str) -> bool {
    file_name
        .strip_prefix(GENERATION_PREFIX)
        .and_then(|rest| rest.strip_prefix(server_id))
        .is_some_and(|rest| rest.starts_with('-') && rest.ends_with(SUFFIX))
}

/// The server ID an endpoint file name ([`endpoint_name`]) carries; None for any other name.
pub fn server_id_of(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(SUFFIX).filter(|id| is_server_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "00000000-0000-4000-8000-000000000000";

    #[test]
    fn protocol_socket_names_round_trip() {
        assert_eq!(server_id_of(&endpoint_name(ID)), Some(ID));
        assert_eq!(server_id_of(&control_name(ID)), None);
        assert_eq!(server_id_of("not-a-uuid.sock"), None);
        let generation = generation_name(ID, "0123456789ab");
        assert_eq!(generation.len(), LONGEST_NAME);
        assert!(is_generation_of(&generation, ID));
        assert!(!is_generation_of(&endpoint_name(ID), ID));
        assert_eq!(server_id_of(&generation), None);
    }
}
