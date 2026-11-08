//! The Rust runtime guide names spawn contracts (decision row 210) only when the host serves them (the durable APIs
//! option; `ULTRON_GUIDE=ts` keeps the TypeScript text). New in Rust: literal expectations.

use resources::runtime_guide::{RuntimeGuideOptions, rlm_runtime_prompt};

#[test]
fn resources_runtime_guide_names_spawn_contracts_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let sentence = r#"Contracts: `rlm.spawn(brief, name=..., contract={"checks": [{"run": "cargo test -q"}, {"changed_files_within": ["src/**"]}]}, hop={"retry": 1})` makes the host run the checks itself when the child ends, and its result's `check["contract"]["verdict"]` is "verified" or "contract_failed" (`help(rlm.spawn)`)."#;
    assert!(rust.contains(sentence), "{rust}");
    assert!(!plain.contains("contract_failed"));
    // It is in the delegation section.
    let delegation = rust.find("## Delegation").unwrap();
    let next = delegation + rust[delegation..].find("\n## ").unwrap();
    assert!(rust[delegation..next].contains(sentence));
}

/// The `schedule` API (kernel_apis::prompt_schedule, Rust host only) is one pointer line in the Rust guide's other
/// APIs; the TypeScript guide never names it.
#[test]
fn resources_runtime_guide_points_at_schedule_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let line = r#"- `await schedule.every("1h", prompt)` or `schedule.at(when, prompt)` sends this session `prompt` later as a new turn (`help(schedule)`)."#;
    assert!(rust.contains(&format!("{line}\n- `await diagram.mermaid(")), "{rust}");
    assert!(!plain.contains("schedule.every"));
    assert_eq!(rust.len() - rust.replace(line, "").len(), line.len());
}

/// `diagram.mermaid` (the diagram crate, Rust host only) is the last pointer line of the Rust guide; the
/// TypeScript guide never names it.
#[test]
fn resources_runtime_guide_points_at_diagram_mermaid_only_with_the_rust_apis() {
    let plain = rlm_runtime_prompt(&["rlm"], &RuntimeGuideOptions::default()).unwrap();
    let rust = rlm_runtime_prompt(
        &["rlm"],
        &RuntimeGuideOptions {
            durable_apis: true,
            ..Default::default()
        },
    )
    .unwrap();
    let line = r#"- `await diagram.mermaid(text)` renders Mermaid to an SVG and PNG in your theme and shows the user the PNG; you get its path (`help(diagram)`)."#;
    assert!(rust.ends_with(&format!("\n{line}")), "{rust}");
    assert!(!plain.contains("diagram"));
    assert_eq!(rust.matches("diagram.mermaid").count(), 1);
}

/// How each lane is told to wait (session_core::wake): the woken root ends its turn (its `rlm.collect` never
/// waits), a root nothing wakes awaits before replying, a subagent awaits before its reply ends its run and has no
/// `schedule` (the root's only), and an in-memory subagent has no swarm APIs either.
#[test]
fn resources_runtime_guide_wording_per_lane_mode() {
    use resources::runtime_guide::{EventDelivery, GuideReader};
    let guide = |delivery, reader, host_waits| {
        rlm_runtime_prompt(
            &["rlm"],
            &RuntimeGuideOptions {
                delivery,
                reader,
                host_waits,
                durable_apis: true,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let woken = guide(EventDelivery::Wake, GuideReader::Root, true);
    assert!(woken.contains(
        "With nothing of your own left, end your turn: each end arrives as a `child_done` event, and `await rlm.collect(hs)` then returns the results (it does not wait: while a selected child runs it raises). Trust only"
    ));
    assert!(woken.contains("which starts a new turn if yours ended"));
    assert!(woken.contains("`await schedule.every(\"1h\", prompt)`"));
    // TypeScript's wording, whose collect waits.
    let ts = guide(EventDelivery::Wake, GuideReader::Root, false);
    assert!(ts.contains("With nothing of your own left, `await rlm.collect(hs)` (free), or end your turn: each end arrives as a `child_done` event. Trust only"));
    for delivery in [EventDelivery::NextCall, EventDelivery::Off] {
        let awaiting = guide(delivery, GuideReader::Root, true);
        assert!(awaiting.contains("With nothing of your own left, `await rlm.collect(hs)` (free). Trust only"));
        assert!(!awaiting.contains("end your turn: each end"));
    }
    let child = guide(EventDelivery::Wake, GuideReader::Subagent { swarm: true }, true);
    assert!(child.contains(
        "Nothing needs polling, and nothing wakes you: your reply ends your run. Before replying, await what your answer still needs (`await rlm.collect(hs)` waits for your subagents, `await job.result()` for a job; waiting costs no model turns)."
    ));
    assert!(child.contains(
        "A command still running after 30 s continues as a job (`out.job`; `await out.job.result()` waits for it)"
    ));
    assert!(child.contains("With nothing of your own left, `await rlm.collect(hs)` (free). Trust only"));
    assert!(!child.contains("starts a new turn"));
    assert!(!child.contains("end your turn"));
    assert!(!child.contains("schedule."));
    assert!(child.contains("`await swarm.route(question)`"));
    assert!(child.contains("`await rlm.images.generate("));
    let memory = guide(EventDelivery::Wake, GuideReader::Subagent { swarm: false }, true);
    assert!(!memory.contains("swarm."));
    assert!(!memory.contains("keep=True"));
    assert!(!memory.contains("schedule."));
    assert!(memory.contains("`await diagram.mermaid(text)`"));
    // With events off a subagent is told so, like the root.
    let off_child = guide(EventDelivery::Off, GuideReader::Subagent { swarm: true }, false);
    assert!(off_child.contains("Completions are not announced in this session (ULTRON_ASYNC_EVENTS=off)"));
}

/// The Rust host's newer APIs (sessions, judge, relations, procedure skills, budget, progress, retrieve context) are
/// named once each to every durable reader, an in-memory subagent included, and never in the TypeScript guide.
/// Memory: every reader is told what it is for; only a weaker tier (MemoryGuidance::PurposeAndProcedure) also how
/// to go about it; topic pages only on memorantado; the result footer only when the setting turns it on.
#[test]
fn resources_runtime_guide_new_apis_memory_tiers_and_footer() {
    use resources::runtime_guide::{EventDelivery, GuideReader, MemoryGuidance};
    let guide = |options: RuntimeGuideOptions| rlm_runtime_prompt(&["rlm"], &options).unwrap();
    let durable = |reader| RuntimeGuideOptions {
        reader,
        durable_apis: true,
        host_waits: true,
        delivery: EventDelivery::Wake,
        ..Default::default()
    };
    let needles = [
        "`await sessions.search(",
        "`await judge.decide(",
        "`await knowledge.relations(",
        "`run = await skills.run(",
        "`await budget.estimate(plan)`",
        "`await rlm.progress()`",
        "`context=\"retrieve\"`",
        "Long-term memory is for what you cannot see next time",
    ];
    let plain = guide(RuntimeGuideOptions::default());
    for reader in [
        GuideReader::Root,
        GuideReader::Subagent { swarm: true },
        GuideReader::Subagent { swarm: false },
    ] {
        let text = guide(durable(reader));
        for needle in needles {
            assert_eq!(text.matches(needle).count(), 1, "{reader:?} {needle}");
            assert!(!plain.contains(needle), "{needle}");
        }
        assert!(
            !text.contains("check memory for it"),
            "{reader:?}: no procedure by default"
        );
        assert!(!text.contains("memory.topic"));
        assert!(!text.contains("[t+"));
    }
    let procedure =
        " Before answering about a topic, check memory for it (`memory.prepare`); test a theory before you keep it.";
    let weak = guide(RuntimeGuideOptions {
        memory: MemoryGuidance::PurposeAndProcedure,
        ..durable(GuideReader::Subagent { swarm: true })
    });
    assert!(
        weak.contains(&format!(
            "call these for anything else.{procedure} A fact tied to files"
        )),
        "{weak}"
    );
    let topics = guide(RuntimeGuideOptions {
        memory_topics: true,
        ..durable(GuideReader::Root)
    });
    assert!(topics.contains(
        "once that text is gone. `memory.topic()` lists the topic pages dreaming wrote, `memory.topic(name)` reads one.\n"
    ));
    let footer =
        "Each rlm result ends with `[t+<time since the session started> ctx <tokens of the request that ran it>]`.";
    let on = guide(RuntimeGuideOptions {
        result_footer: true,
        ..durable(GuideReader::Root)
    });
    assert!(
        on.contains(&format!(
            "re-create imports and functions.\n{footer}\nNothing needs polling"
        )),
        "{on}"
    );
    // The TypeScript guide never changes: none of the switches reach it.
    let ts = guide(RuntimeGuideOptions {
        memory: MemoryGuidance::PurposeAndProcedure,
        memory_topics: true,
        result_footer: true,
        ..Default::default()
    });
    assert_eq!(ts, plain);
}
