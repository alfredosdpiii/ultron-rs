//! `knowledge.*` for a session ([`graph`]): the knowledge store under the agent directory, the session
//! directory's git repository, the global `graph.neo4j.*` and `memory.*` settings read at each call, and Neo4j's
//! password from the session's environment (`NEO4J_PASSWORD`) or the profile's auth.json (`neo4j`).

use crate::compose::Config;
use std::sync::Arc;

/// The knowledge module of a session.
pub fn module(config: &Arc<Config>) -> graph::KnowledgeModule {
    let settings = config.settings_reader();
    let auth = config::paths::AgentPaths::new(&config.agent_dir).auth();
    graph::KnowledgeModule::new(
        knowledge::default_path(&config.agent_dir),
        config.cwd.clone(),
        Arc::new(move || {
            let settings = settings();
            let global = settings.global_settings();
            graph::Mirrors {
                neo4j: config::keys::neo4j_settings(global),
                memorantado: config::keys::memorantado_settings(global),
            }
        }),
        Arc::new(|| {
            let transport: Arc<dyn blocking_http::Transport> = Arc::new(blocking_http::ReqwestTransport::new()?);
            Ok(transport)
        }),
        judge::keys::key_source("neo4j", config.var.clone(), auth),
    )
}
