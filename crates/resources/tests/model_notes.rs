use resources::model_notes::model_notes;

#[test]
fn resources_model_notes_follow_the_model_family() {
    let fable = model_notes("claude-fable-5.2").unwrap();
    assert_eq!(fable.lines().count(), 3);
    assert!(fable.contains("asyncio.gather"));
    // The same model under its dashed id, and a dated snapshot of it.
    assert_eq!(
        model_notes("claude-fable-5-2-20261001").as_deref(),
        Some(fable.as_str())
    );
    let first = model_notes("claude-fable-5").unwrap();
    assert_eq!(first.lines().count(), 2);
    assert!(!first.contains("asyncio.gather"));
    assert_eq!(model_notes("claude-sonnet-5-5").unwrap().lines().count(), 1);
    assert_eq!(model_notes("claude-sonnet-4-6"), None);
    assert_eq!(model_notes("gpt-6-sol"), None);
}
