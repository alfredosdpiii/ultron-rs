//! The kernel lane names of delegated work: a subagent acts as `ultron.rlm-child.<task id>` and a typed agent's
//! `rlm` frame as `ultron.rlm-frame.<task id>`; usage reports and session counters read the work back from them.

pub const CHILD_LANE_PREFIX: &str = "ultron.rlm-child.";
pub const FRAME_LANE_PREFIX: &str = "ultron.rlm-frame.";

/// The lane subagent `task_id` acts as.
pub fn child_lane(task_id: &str) -> String {
    format!("{CHILD_LANE_PREFIX}{task_id}")
}

/// The lane the `rlm` frame of typed task `task_id` acts as.
pub fn frame_lane(task_id: &str) -> String {
    format!("{FRAME_LANE_PREFIX}{task_id}")
}

/// Whose work a lane name is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneOwner<'a> {
    Child(&'a str),
    Frame(&'a str),
}

/// The task a delegated lane belongs to; `None` for any other lane.
pub fn parse_lane(lane: &str) -> Option<LaneOwner<'_>> {
    if let Some(id) = lane.strip_prefix(CHILD_LANE_PREFIX) {
        return Some(LaneOwner::Child(id));
    }
    lane.strip_prefix(FRAME_LANE_PREFIX).map(LaneOwner::Frame)
}
