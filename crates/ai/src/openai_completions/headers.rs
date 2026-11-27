use super::compat::Compat;
use crate::http::HeaderBuilder;
use crate::options::StreamOptions;
use crate::provider::openai_client_headers;
use crate::types::{Model, TranscriptContext};

pub(super) fn client_headers(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    api_key: &str,
    session_id: Option<&str>,
    compat: &Compat,
) -> HeaderBuilder {
    let affinity_format = compat
        .send_session_affinity_headers
        .then_some(compat.session_affinity_format.as_str());
    openai_client_headers(model, context, options, api_key, session_id, affinity_format, true)
}
