use serde_json::Value;
use stitch::*;

#[derive(Default)]
pub struct BranchObj {
    pub entries: Vec<Value>,
    pub ids: Vec<String>,
    pub seq: i64,
}

object!(Branch, BranchObj);
datatype!(MsgText);
