//! How much the memory section of a reader's guide says, by the reader's model tier (the memory guidance of the
//! plan): every reader gets the purpose line (what memory is for); a weaker tier also gets the procedural line
//! (check memory for the topic before answering; test a theory before keeping it). A strong root model does
//! better with the purpose alone, so procedure is spent only where it helps. The guide's owner maps each value to
//! its text in the runtime guide; this decides which one a reader gets.

use ai::Model;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryGuidance {
    /// What memory is for, and nothing on how to go about it.
    Purpose,
    /// The purpose and the procedural line.
    PurposeAndProcedure,
}

/// The guidance of a reader on `model` in a session whose root runs `root`: the root lane, and a child on the
/// root's model (or one priced as high), get the purpose only; a child or job on a cheaper model (its input price
/// below the root's, a free local model under a paid root included) gets the procedure too.
pub fn memory_guidance(model: &Model, root: &Model, subagent: bool) -> MemoryGuidance {
    let same = model.provider == root.provider && model.id == root.id;
    if subagent && !same && model.cost.input < root.cost.input {
        MemoryGuidance::PurposeAndProcedure
    } else {
        MemoryGuidance::Purpose
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, input: f64) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "api": "openai-completions", "provider": "p", "baseUrl": "http://x",
            "reasoning": false, "input": ["text"], "contextWindow": 1000, "maxTokens": 100,
            "cost": {"input": input, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        }))
        .unwrap()
    }

    #[test]
    fn memory_guidance_adds_procedure_for_cheaper_children_only() {
        let root = model("big", 15.0);
        assert_eq!(memory_guidance(&root, &root, false), MemoryGuidance::Purpose);
        assert_eq!(
            memory_guidance(&root, &root, true),
            MemoryGuidance::Purpose,
            "a child on the root's model"
        );
        let cheap = model("small", 0.8);
        assert_eq!(
            memory_guidance(&cheap, &root, true),
            MemoryGuidance::PurposeAndProcedure
        );
        assert_eq!(
            memory_guidance(&cheap, &root, false),
            MemoryGuidance::Purpose,
            "the root itself"
        );
        assert_eq!(
            memory_guidance(&model("local", 0.0), &root, true),
            MemoryGuidance::PurposeAndProcedure
        );
        assert_eq!(
            memory_guidance(&model("peer", 15.0), &root, true),
            MemoryGuidance::Purpose
        );
    }
}
