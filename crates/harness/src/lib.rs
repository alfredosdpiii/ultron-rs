//! Durable task runtime. Ported so far: Ultron's native task journal
//! (`packages/coding-agent/src/ultron/rlm/task-store.ts`), the lane snapshot reducer
//! (`packages/agent/src/harness/runtime/reducer.ts`), the upstream Pi Durable scheduler
//! (`scheduler`) and its conversation runtime (`chat`: submissions, inbox, generation, durable REPL cells, compaction,
//! delegated children, swarm state); see PLAN.md for the rest.

pub mod chat;
pub mod reducer;
pub mod scheduler;
pub mod stream_fold;
pub mod task_store;
