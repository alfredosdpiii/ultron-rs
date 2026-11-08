//! Model prices for `budget.estimate` and the stop-line of workflow nodes (`delegation::estimate`): a unit's own
//! model, else its kind's default in this session, priced from the built-in catalog or models.json. A judge has no
//! catalog model (its backends are services): an unnamed one is priced at `judge.jev.pricePerMTok` when Jev is one
//! of its backends (as the seat prices Jev's answers), else it stays unpriced.

use crate::RunEnv as Env;
use crate::compose::Config;
use ai::Model;
use delegation::estimate::{Priced, Pricing, Rates};
use jsrt::sync;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Prices units of this session: a subagent defaults to `rlm.childModel`, a frame to `ULTRON_RLM_FRAME_MODEL` or
/// `rlm.frameModel` (as frames resolve them), and each of them and a completion to the session's model as it is now.
pub fn pricing(config: &Config, root_model: Arc<Mutex<Model>>) -> Pricing {
    let env = Env {
        cwd: config.cwd.clone(),
        home: config.home.clone(),
        var: config.var.clone(),
        package: PathBuf::new(),
    };
    let settings = config.settings_reader();
    let frame_env = config.frame_env.clone();
    Arc::new(move |kind: &str, model: Option<&str>| {
        let configured = || {
            let rlm = config::keys::rlm_model_settings(settings().global_settings());
            match kind {
                "child" => rlm.child_model,
                "frame" => config::env::env_model(&|key| frame_env.get(key).cloned(), config::env::RLM_FRAME_MODEL_ENV)
                    .or(rlm.frame_model),
                _ => None,
            }
        };
        let spec = match model {
            Some(model) => model.to_string(),
            None if kind == "judge" => {
                let judge = config::keys::judge_settings(settings().global_settings());
                return match judge
                    .jev_price_per_mtok
                    .filter(|_| judge.providers.iter().any(|p| p == "jev"))
                {
                    Some(price) => Priced {
                        model: Some(judge.model),
                        rates: Some(Rates {
                            input: price,
                            output: price,
                        }),
                    },
                    None => Priced::default(),
                };
            }
            None => match configured() {
                Some(model) => model,
                None => {
                    let root = sync::lock(&root_model).clone();
                    return Priced {
                        model: Some(format!("{}/{}", root.provider, root.id)),
                        rates: Some(Rates::of(&root)),
                    };
                }
            },
        };
        let found = spec
            .split_once('/')
            .and_then(|(provider, id)| crate::lane::catalog_model(provider, id, &env).ok());
        Priced {
            rates: found.as_ref().map(Rates::of),
            model: Some(spec),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A judge unit is priced at `judge.jev.pricePerMTok` while Jev is one of the judge's backends, as the seat
    /// prices Jev's answers; otherwise it stays unpriced.
    #[test]
    fn session_core_budget_prices_a_judge_unit_at_the_jev_price() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let config_with = |judge: serde_json::Value| {
            std::fs::write(
                agent.join("settings.json"),
                serde_json::json!({"judge": judge}).to_string(),
            )
            .unwrap();
            let agent = agent.display().to_string();
            Config::from_env(&Env {
                cwd: dir.path().to_path_buf(),
                home: dir.path().to_path_buf(),
                var: Arc::new(move |key| (key == config::paths::ENV_AGENT_DIR).then(|| agent.clone())),
                package: PathBuf::new(),
            })
        };
        let root = Arc::new(Mutex::new(
            ai::models::builtin_models("anthropic").next().unwrap().clone(),
        ));
        let priced = pricing(
            &config_with(serde_json::json!({"jev": {"pricePerMTok": 0.4}})),
            root.clone(),
        );
        assert_eq!(
            priced("judge", None),
            Priced {
                model: Some(config::keys::JUDGE_DEFAULT_MODEL.into()),
                rates: Some(Rates {
                    input: 0.4,
                    output: 0.4
                }),
            }
        );
        let d1_only = pricing(
            &config_with(serde_json::json!({"provider": ["d1"], "jev": {"pricePerMTok": 0.4}})),
            root.clone(),
        );
        assert_eq!(d1_only("judge", None), Priced::default());
        assert_eq!(
            pricing(&config_with(serde_json::json!({})), root)("judge", None),
            Priced::default()
        );
    }
}
