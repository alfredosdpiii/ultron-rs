//! Questions and answers: what a caller asks (a closed answer set per question), the System One request spec Jev
//! and d1 share, their answers read back, and the JSON schema the chat rung answers through.
//!
//! System One question types: `choice` (`criteria` as `{label: description}`, answered `{choice, confidence,
//! probabilities}`), `noul` (a yes/no answered `{noul: p}`, the probability of yes) and `score` (`criteria` as a
//! list of described levels, answered `{score: 1..=n, confidence}`). `number` (`range`, `step`, answered
//! `{number, confidence}`) and `multiple` choices (`choices`, else every criterion whose probability is at least
//! 0.5) are sent in the same style but were not measured against either service.

use serde_json::{Map, Value, json};

/// The answer set of one question.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// One label (or, with `multiple`, any subset) of `criteria`, `(label, description)` in order.
    Choice {
        criteria: Vec<(String, String)>,
        multiple: bool,
    },
    YesNo,
    /// A level `1..=levels.len()`, each level described.
    Score {
        levels: Vec<String>,
    },
    /// A number in `min..=max`, on `step` when given.
    Number {
        min: f64,
        max: f64,
        step: Option<f64>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub name: String,
    pub instructions: String,
    pub kind: Kind,
}

/// One answer: `value` is a label (a list of labels for a multiple choice), a bool, a level or a number.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub value: Value,
    /// Per label (`yes`/`no` for a yes/no), when the backend reports them.
    pub probabilities: Option<Map<String, Value>>,
    /// How sure the backend is of `value`, 0..=1; a yes/no's is `max(p, 1 - p)`.
    pub confidence: f64,
}

impl Answer {
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("value".into(), self.value.clone());
        out.insert("confidence".into(), json!(self.confidence));
        if let Some(probabilities) = &self.probabilities {
            out.insert("probabilities".into(), Value::Object(probabilities.clone()));
        }
        Value::Object(out)
    }
}

const MAX_QUESTIONS: usize = 16;
const MAX_LABELS: usize = 32;

fn text(value: Option<&Value>, what: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !jsrt::string::trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{what} must be a nonempty string")),
    }
}

fn number(value: Option<&Value>, what: &str) -> Result<f64, String> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("{what} must be a finite number"))
}

/// One question from its spec: `{"type": "choice"|"yesno"|"score"|"number", "instructions", ...}` (`noul` is read as
/// `yesno`, as System One names it).
fn question(name: &str, spec: &Value) -> Result<Question, String> {
    let at = |key: &str| format!("questions.{name}.{key}");
    let spec = spec
        .as_object()
        .ok_or_else(|| format!("questions.{name} must be an object"))?;
    let kind = text(spec.get("type"), &at("type"))?;
    let allowed: &[&str] = match kind.as_str() {
        "choice" => &["type", "instructions", "criteria", "multiple"],
        "yesno" | "noul" => &["type", "instructions"],
        "score" => &["type", "instructions", "levels"],
        "number" => &["type", "instructions", "range", "step"],
        other => {
            return Err(format!(
                "{} must be choice, yesno, score or number, not {other}",
                at("type")
            ));
        }
    };
    jsrt::payload::fields(spec, allowed).map_err(|e| format!("questions.{name}: {e}"))?;
    let instructions = text(spec.get("instructions"), &at("instructions"))?;
    let kind = match kind.as_str() {
        "choice" => {
            let criteria = spec
                .get("criteria")
                .and_then(Value::as_object)
                .filter(|c| (2..=MAX_LABELS).contains(&c.len()))
                .ok_or_else(|| format!("{} must map 2 to {MAX_LABELS} labels to descriptions", at("criteria")))?;
            let criteria = criteria
                .iter()
                .map(|(label, description)| {
                    let description = text(Some(description), &at(&format!("criteria.{label}")))?;
                    Ok((label.clone(), description))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let multiple = match spec.get("multiple") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => return Err(format!("{} must be true or false", at("multiple"))),
            };
            Kind::Choice { criteria, multiple }
        }
        "score" => {
            let levels = spec
                .get("levels")
                .and_then(Value::as_array)
                .filter(|l| (2..=MAX_LABELS).contains(&l.len()))
                .ok_or_else(|| format!("{} must be a list of 2 to {MAX_LABELS} described levels", at("levels")))?;
            let levels = levels
                .iter()
                .enumerate()
                .map(|(i, level)| text(Some(level), &at(&format!("levels[{i}]"))))
                .collect::<Result<Vec<_>, String>>()?;
            Kind::Score { levels }
        }
        "number" => {
            let range = spec
                .get("range")
                .and_then(Value::as_array)
                .filter(|r| r.len() == 2)
                .ok_or_else(|| format!("{} must be [min, max]", at("range")))?;
            let (min, max) = (
                number(range.first(), &at("range"))?,
                number(range.get(1), &at("range"))?,
            );
            if min >= max {
                return Err(format!("{} must have min below max", at("range")));
            }
            let step = match spec.get("step") {
                None | Some(Value::Null) => None,
                step => Some(number(step, &at("step"))?)
                    .filter(|s| *s > 0.0)
                    .map(Some)
                    .ok_or_else(|| format!("{} must be a positive number", at("step")))?,
            };
            Kind::Number { min, max, step }
        }
        _ => Kind::YesNo,
    };
    Ok(Question {
        name: name.to_string(),
        instructions,
        kind,
    })
}

/// The questions of a request: an object of 1 to 16 named question specs.
pub fn parse_questions(value: &Value) -> Result<Vec<Question>, String> {
    let specs = value
        .as_object()
        .filter(|q| (1..=MAX_QUESTIONS).contains(&q.len()))
        .ok_or_else(|| format!("questions must be an object of 1 to {MAX_QUESTIONS} named questions"))?;
    specs.iter().map(|(name, spec)| question(name, spec)).collect()
}

/// The System One `questions` object.
pub fn wire_questions(questions: &[Question]) -> Value {
    let mut out = Map::new();
    for q in questions {
        let spec = match &q.kind {
            Kind::Choice { criteria, multiple } => {
                let criteria: Map<String, Value> = criteria.iter().map(|(l, d)| (l.clone(), json!(d))).collect();
                let mut spec = json!({"type": "choice", "instructions": q.instructions, "criteria": criteria});
                if *multiple {
                    spec["multiple"] = json!(true);
                }
                spec
            }
            Kind::YesNo => json!({"type": "noul", "instructions": q.instructions}),
            Kind::Score { levels } => json!({"type": "score", "instructions": q.instructions, "criteria": levels}),
            Kind::Number { min, max, step } => {
                let mut spec = json!({"type": "number", "instructions": q.instructions, "range": [min, max]});
                if let Some(step) = step {
                    spec["step"] = json!(step);
                }
                spec
            }
        };
        out.insert(q.name.clone(), spec);
    }
    Value::Object(out)
}

fn unit(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| (0.0..=1.0).contains(n))
}

fn probabilities(answer: &Map<String, Value>) -> Option<Map<String, Value>> {
    let map = answer.get("probabilities")?.as_object()?;
    map.values().all(|p| unit(Some(p)).is_some()).then(|| map.clone())
}

fn in_range(n: f64, min: f64, max: f64, step: Option<f64>) -> bool {
    let on_step = step.is_none_or(|s| {
        let k = (n - min) / s;
        (k - k.round()).abs() < 1e-6
    });
    (min..=max).contains(&n) && on_step
}

/// One System One answer read back for `q`; `None` when it does not answer the question asked.
fn wire_answer(q: &Question, answer: &Value) -> Option<Answer> {
    let answer = answer.as_object()?;
    let probabilities = probabilities(answer);
    let reported = unit(answer.get("confidence"));
    match &q.kind {
        Kind::YesNo => {
            let p = unit(answer.get("noul"))?;
            let mut map = Map::new();
            map.insert("yes".into(), json!(p));
            map.insert("no".into(), json!(1.0 - p));
            Some(Answer {
                value: json!(p >= 0.5),
                probabilities: Some(map),
                confidence: p.max(1.0 - p),
            })
        }
        Kind::Choice {
            criteria,
            multiple: false,
        } => {
            let choice = answer.get("choice")?.as_str()?;
            criteria.iter().any(|(l, _)| l == choice).then_some(())?;
            let confidence = reported.or_else(|| unit(probabilities.as_ref()?.get(choice)))?;
            Some(Answer {
                value: json!(choice),
                probabilities,
                confidence,
            })
        }
        Kind::Choice {
            criteria,
            multiple: true,
        } => {
            let p_of = |label: &str| unit(probabilities.as_ref()?.get(label));
            let picked: Vec<String> = match answer.get("choices").and_then(Value::as_array) {
                Some(list) => list
                    .iter()
                    .map(|c| c.as_str().map(str::to_string))
                    .collect::<Option<_>>()?,
                None => criteria
                    .iter()
                    .filter(|(l, _)| p_of(l).is_some_and(|p| p >= 0.5))
                    .map(|(l, _)| l.clone())
                    .collect(),
            };
            picked
                .iter()
                .all(|p| criteria.iter().any(|(l, _)| l == p))
                .then_some(())?;
            let confidence = reported.or_else(|| {
                criteria
                    .iter()
                    .map(|(l, _)| p_of(l).map(|p| p.max(1.0 - p)))
                    .try_fold(1.0f64, |least, p| p.map(|p| least.min(p)))
            })?;
            Some(Answer {
                value: json!(picked),
                probabilities,
                confidence,
            })
        }
        Kind::Score { levels } => {
            let score = answer.get("score")?.as_f64()?;
            (score.fract() == 0.0 && score >= 1.0 && score <= levels.len() as f64).then_some(())?;
            Some(Answer {
                value: json!(score as u64),
                probabilities,
                confidence: reported?,
            })
        }
        Kind::Number { min, max, step } => {
            let n = answer.get("number").or_else(|| answer.get("value"))?.as_f64()?;
            in_range(n, *min, *max, *step).then_some(())?;
            Some(Answer {
                value: json!(n),
                probabilities,
                confidence: reported?,
            })
        }
    }
}

/// The System One `answers` object read back: one valid answer per question asked, else `Err` naming the first
/// question it does not answer.
pub fn wire_answers(questions: &[Question], answers: &Value) -> Result<Vec<Answer>, String> {
    questions
        .iter()
        .map(|q| wire_answer(q, &answers[&q.name]).ok_or_else(|| format!("no valid answer to {}", q.name)))
        .collect()
}

/// The JSON schema of one question's `value` (the chat rung's closed answer set).
fn value_schema(q: &Question) -> Value {
    let labels = |criteria: &[(String, String)]| criteria.iter().map(|(l, _)| json!(l)).collect::<Vec<_>>();
    match &q.kind {
        Kind::Choice {
            criteria,
            multiple: false,
        } => json!({"type": "string", "enum": labels(criteria)}),
        Kind::Choice {
            criteria,
            multiple: true,
        } => json!({"type": "array", "items": {"type": "string", "enum": labels(criteria)}}),
        Kind::YesNo => json!({"type": "boolean"}),
        Kind::Score { levels } => json!({"type": "integer", "minimum": 1, "maximum": levels.len()}),
        Kind::Number { min, max, .. } => json!({"type": "number", "minimum": min, "maximum": max}),
    }
}

/// The parameters of the chat rung's `answer` tool: per question, `{value, confidence}`.
pub fn chat_schema(questions: &[Question]) -> Value {
    let mut properties = Map::new();
    for q in questions {
        properties.insert(
            q.name.clone(),
            json!({
                "type": "object",
                "properties": {
                    "value": value_schema(q),
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                },
                "required": ["value", "confidence"],
                "additionalProperties": false,
            }),
        );
    }
    let names: Vec<&str> = questions.iter().map(|q| q.name.as_str()).collect();
    json!({"type": "object", "properties": properties, "required": names, "additionalProperties": false})
}

/// The questions as the chat rung reads them.
pub fn chat_prompt(state: &str, questions: &[Question]) -> String {
    let mut out = format!("STATE:\n{state}\n\nQUESTIONS:\n");
    for q in questions {
        let (kind, options) = match &q.kind {
            Kind::Choice { criteria, multiple } => (
                if *multiple { "choose any number" } else { "choose one" },
                criteria
                    .iter()
                    .map(|(l, d)| format!("    {l}: {d}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Kind::YesNo => ("true or false", String::new()),
            Kind::Score { levels } => (
                "a level",
                levels
                    .iter()
                    .enumerate()
                    .map(|(i, d)| format!("    {}: {d}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Kind::Number { min, max, step } => (
                "a number",
                format!(
                    "    from {min} to {max}{}",
                    step.map_or(String::new(), |s| format!(" in steps of {s}"))
                ),
            ),
        };
        out.push_str(&format!("- {} ({kind}): {}\n", q.name, q.instructions));
        if !options.is_empty() {
            out.push_str(&options);
            out.push('\n');
        }
    }
    out
}

/// The chat rung's `answer` arguments read back: every question answered within its set, with a confidence.
pub fn chat_answers(questions: &[Question], arguments: &Value) -> Result<Vec<Answer>, String> {
    questions
        .iter()
        .map(|q| {
            let answer = &arguments[&q.name];
            let confidence = unit(answer.get("confidence"));
            let value = answer.get("value").cloned().unwrap_or(Value::Null);
            let valid = match &q.kind {
                Kind::Choice {
                    criteria,
                    multiple: false,
                } => value.as_str().is_some_and(|v| criteria.iter().any(|(l, _)| l == v)),
                Kind::Choice {
                    criteria,
                    multiple: true,
                } => value.as_array().is_some_and(|items| {
                    items
                        .iter()
                        .all(|v| v.as_str().is_some_and(|v| criteria.iter().any(|(l, _)| l == v)))
                }),
                Kind::YesNo => value.is_boolean(),
                Kind::Score { levels } => value.as_u64().is_some_and(|n| n >= 1 && n <= levels.len() as u64),
                Kind::Number { min, max, step } => value.as_f64().is_some_and(|n| in_range(n, *min, *max, *step)),
            };
            match (valid, confidence) {
                (true, Some(confidence)) => Ok(Answer {
                    value,
                    probabilities: None,
                    confidence,
                }),
                _ => Err(format!("no valid answer to {}", q.name)),
            }
        })
        .collect()
}
