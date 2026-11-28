pub use crate::objects::*;
use serde_json::{Value, json};
use session_journal::context::{
    CONTEXT_EDIT_CUSTOM_TYPE, apply_context_edits, apply_context_edits_marked, build_context_entries,
    build_session_context, convert_to_llm,
};
use session_journal::search::{SpanRef, entry_text};
use stitch::*;

endpoint! {
    fn new_branch() -> New<Branch> {
        New::new(BranchObj::default())
    }
}

endpoint! {
    fn push_user(b: Borrow<Branch>, text: Str<256, MsgText>) {
        if b.entries.len() >= 80 {
            return;
        }
        b.seq += 1;
        let seq = b.seq;
        let id = format!("u{seq}");
        let entry = json!({
            "id": id,
            "parentId": null,
            "seq": seq,
            "timestamp": seq,
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": &*text}]}
        });
        b.entries.push(entry);
        b.ids.push(id);
    }
}

endpoint! {
    fn push_tool_pair(b: Borrow<Branch>, output: Str<512, MsgText>) {
        if b.entries.len() >= 78 {
            return;
        }
        b.seq += 1;
        let seq1 = b.seq;
        let call_id = format!("tc{seq1}");
        let a_id = format!("a{seq1}");
        let a_entry = json!({
            "id": a_id,
            "parentId": null,
            "seq": seq1,
            "timestamp": seq1,
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "toolCall", "id": call_id, "name": "rlm", "arguments": {"code": "1+1"}}]
            }
        });
        b.entries.push(a_entry);
        b.seq += 1;
        let seq2 = b.seq;
        let r_id = format!("r{seq2}");
        let r_entry = json!({
            "id": r_id,
            "parentId": a_id,
            "seq": seq2,
            "timestamp": seq2,
            "type": "message",
            "message": {
                "role": "toolResult",
                "toolCallId": call_id,
                "toolName": "rlm",
                "content": [{"type": "text", "text": &*output}]
            }
        });
        b.entries.push(r_entry);
        b.ids.push(a_id);
        b.ids.push(r_id);
    }
}

endpoint! {
    fn push_context_edit(b: Borrow<Branch>, idx: u8, omit: bool, replacement: Str<128, MsgText>) {
        if b.ids.is_empty() || b.entries.len() >= 85 {
            return;
        }
        let target = b.ids[(idx as usize) % b.ids.len()].clone();
        b.seq += 1;
        let seq = b.seq;
        let id = format!("e{seq}");
        let rep = if omit {
            Value::Null
        } else {
            json!({"content": &*replacement})
        };
        let entry = json!({
            "id": id,
            "parentId": null,
            "seq": seq,
            "timestamp": seq,
            "type": "custom",
            "customType": CONTEXT_EDIT_CUSTOM_TYPE,
            "data": {"edits": [{"targetId": target, "replacement": rep}], "source": "fuzz"}
        });
        b.entries.push(entry);
    }
}

endpoint! {
    fn push_compaction(b: Borrow<Branch>, summary: Str<128, MsgText>) {
        if b.entries.len() >= 85 {
            return;
        }
        b.seq += 1;
        let seq = b.seq;
        let id = format!("cmp{seq}");
        let entry = json!({
            "id": id,
            "parentId": null,
            "seq": seq,
            "timestamp": seq,
            "type": "compaction",
            "summary": &*summary,
            "retainedTail": []
        });
        b.entries.push(entry);
    }
}

endpoint! {
    fn check_apply_and_project(b: Borrow<Branch>, tool_chars: Range<0, 512>) {
        let applied = apply_context_edits(&b.entries);
        let marked = apply_context_edits_marked(&b.entries);
        check(
            applied.len() == marked.len(),
            "apply_context_edits and apply_context_edits_marked length mismatch",
        );
        let built = build_context_entries(&b.entries);
        let session_msgs = build_session_context(&b.entries);
        let _ = convert_to_llm(&session_msgs);
        for entry in &built {
            let _ = entry_text(entry, *tool_chars as usize);
        }
    }
}

endpoint! {
    fn fuzz_span_parse(text: Str<128, MsgText>) {
        let _ = SpanRef::parse(&text);
    }
}
