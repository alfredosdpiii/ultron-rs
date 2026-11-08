//! Questions: validation, the System One wire shapes (as judgebench's cases.json sends them), answers read back,
//! and the chat rung's closed answer set.

use judge::question::{Kind, chat_answers, chat_schema, parse_questions, wire_answers, wire_questions};
use serde_json::json;

#[test]
fn judge_questions_are_validated() {
    for (spec, error) in [
        (json!({}), "1 to 16"),
        (
            json!({"q": {"type": "maybe", "instructions": "x"}}),
            "choice, yesno, score or number",
        ),
        (json!({"q": {"type": "yesno"}}), "questions.q.instructions"),
        (
            json!({"q": {"type": "yesno", "instructions": "x", "criteria": {}}}),
            "Unknown payload field: criteria",
        ),
        (
            json!({"q": {"type": "choice", "instructions": "x", "criteria": {"a": "only one"}}}),
            "2 to 32 labels",
        ),
        (
            json!({"q": {"type": "choice", "instructions": "x", "criteria": {"a": "", "b": "y"}}}),
            "criteria.a",
        ),
        (
            json!({"q": {"type": "score", "instructions": "x", "levels": ["one"]}}),
            "2 to 32 described levels",
        ),
        (
            json!({"q": {"type": "number", "instructions": "x", "range": [5, 1]}}),
            "min below max",
        ),
        (
            json!({"q": {"type": "number", "instructions": "x", "range": [0, 1], "step": 0}}),
            "positive number",
        ),
    ] {
        let got = parse_questions(&spec).unwrap_err();
        assert!(got.contains(error), "{spec}: {got}");
    }
    let parsed = parse_questions(&json!({"q": {"type": "noul", "instructions": "x"}})).unwrap();
    assert_eq!(parsed[0].kind, Kind::YesNo);
}

#[test]
fn judge_questions_go_out_in_system_one() {
    let questions = parse_questions(&json!({
        "memory": {"type": "choice", "instructions": "Still true?", "criteria": {"supported": "s", "stale": "t"}},
        "finding": {"type": "yesno", "instructions": "Shown?"},
        "support": {"type": "score", "instructions": "How strong?", "levels": ["none", "weak", "strong"]},
        "lines": {"type": "number", "instructions": "How many?", "range": [0, 100], "step": 1},
        "tags": {"type": "choice", "instructions": "Which apply?", "criteria": {"a": "x", "b": "y"}, "multiple": true},
    }))
    .unwrap();
    assert_eq!(
        wire_questions(&questions),
        json!({
            "memory": {"type": "choice", "instructions": "Still true?", "criteria": {"supported": "s", "stale": "t"}},
            "finding": {"type": "noul", "instructions": "Shown?"},
            "support": {"type": "score", "instructions": "How strong?", "criteria": ["none", "weak", "strong"]},
            "lines": {"type": "number", "instructions": "How many?", "range": [0.0, 100.0], "step": 1.0},
            "tags": {"type": "choice", "instructions": "Which apply?", "criteria": {"a": "x", "b": "y"}, "multiple": true},
        })
    );
    // The answers read back, each within its set.
    let answers = wire_answers(
        &questions,
        &json!({
            "memory": {"choice": "stale", "confidence": 0.44, "probabilities": {"supported": 0.3, "stale": 0.44}},
            "finding": {"noul": 0.2},
            "support": {"score": 3, "confidence": 0.7},
            "lines": {"number": 42, "confidence": 0.6},
            "tags": {"probabilities": {"a": 0.9, "b": 0.3}},
        }),
    )
    .unwrap();
    let values: Vec<_> = answers.iter().map(|a| a.value.clone()).collect();
    assert_eq!(
        values,
        [json!("stale"), json!(false), json!(3), json!(42.0), json!(["a"])]
    );
    let confidence: Vec<f64> = answers.iter().map(|a| a.confidence).collect();
    assert_eq!(confidence, [0.44, 0.8, 0.7, 0.6, 0.7]);
    assert_eq!(answers[1].to_json()["probabilities"], json!({"yes": 0.2, "no": 0.8}));
}

#[test]
fn judge_answers_outside_the_set_are_refused() {
    let questions = parse_questions(&json!({
        "q": {"type": "choice", "instructions": "x", "criteria": {"a": "x", "b": "y"}},
        "n": {"type": "number", "instructions": "x", "range": [0, 10], "step": 2},
    }))
    .unwrap();
    for answers in [
        json!({"q": {"choice": "c", "confidence": 0.9}, "n": {"number": 4, "confidence": 1}}),
        json!({"q": {"choice": "a"}, "n": {"number": 4, "confidence": 1}}),
        json!({"q": {"choice": "a", "confidence": 0.9}, "n": {"number": 3, "confidence": 1}}),
        json!({"q": {"choice": "a", "confidence": 0.9}, "n": {"number": 12, "confidence": 1}}),
        json!({"q": {"choice": "a", "confidence": 0.9}}),
    ] {
        assert!(wire_answers(&questions, &answers).is_err(), "{answers}");
    }
    assert!(
        wire_answers(
            &questions,
            &json!({"q": {"choice": "a", "probabilities": {"a": 0.9}}, "n": {"value": 4, "confidence": 1}})
        )
        .is_ok()
    );
}

#[test]
fn judge_chat_rung_answers_through_a_closed_schema() {
    let questions = parse_questions(&json!({
        "q": {"type": "choice", "instructions": "x", "criteria": {"reads": "r", "writes": "w"}},
        "ok": {"type": "yesno", "instructions": "y"},
        "s": {"type": "score", "instructions": "z", "levels": ["low", "high"]},
    }))
    .unwrap();
    let schema = chat_schema(&questions);
    assert_eq!(schema["required"], json!(["q", "ok", "s"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["q"]["properties"]["value"],
        json!({"type": "string", "enum": ["reads", "writes"]})
    );
    assert_eq!(
        schema["properties"]["s"]["properties"]["value"],
        json!({"type": "integer", "minimum": 1, "maximum": 2})
    );
    let good = json!({"q": {"value": "reads", "confidence": 0.9}, "ok": {"value": true, "confidence": 1},
        "s": {"value": 2, "confidence": 0.5}});
    assert_eq!(chat_answers(&questions, &good).unwrap()[2].value, json!(2));
    for bad in [
        json!({"q": {"value": "mentions", "confidence": 0.9}, "ok": {"value": true, "confidence": 1}, "s": {"value": 2, "confidence": 0.5}}),
        json!({"q": {"value": "reads"}, "ok": {"value": true, "confidence": 1}, "s": {"value": 2, "confidence": 0.5}}),
        json!({"q": {"value": "reads", "confidence": 0.9}, "ok": {"value": "yes", "confidence": 1}, "s": {"value": 2, "confidence": 0.5}}),
        json!({"q": {"value": "reads", "confidence": 0.9}, "ok": {"value": true, "confidence": 1}, "s": {"value": 3, "confidence": 0.5}}),
    ] {
        assert!(chat_answers(&questions, &bad).is_err(), "{bad}");
    }
}
