//! `SessionLink`'s phase (client::session `LinkPhase`): what the supervisor rejoins for. The link never gives up on
//! its session, so a rejoin that gave up after it replaced the connection (no attachment left) is tried again.

use client::session::{LinkPhase, WORKER_RESTARTED};

#[test]
fn client_link_keeps_rejoining_after_a_lost_rejoin_left_no_attachment() {
    // `try_rejoin` replaced the connection (the attachment is gone), every attach failed, `lost` was reported:
    // the supervisor rejoins again instead of leaving the link idle.
    assert_eq!(LinkPhase::Lost.rejoin_reason(false, false), Some(WORKER_RESTARTED));
    assert_eq!(
        LinkPhase::Lost.rejoin_reason(true, false),
        Some("The Ultron server went away")
    );
    // Attached: rejoin when the server went away or the attachment moved, not otherwise.
    assert_eq!(LinkPhase::Attached.rejoin_reason(false, true), None);
    assert_eq!(LinkPhase::Attached.rejoin_reason(false, false), Some(WORKER_RESTARTED));
    // Nothing to rejoin before the first attach, and nothing while a recovery runs (calls wait for it instead).
    // An owner's switch drops the old attachment before the new one exists: no rejoin of the old session meanwhile.
    for phase in [
        LinkPhase::Unattached,
        LinkPhase::Attaching,
        LinkPhase::Rejoining,
        LinkPhase::RadiusRecovering,
    ] {
        assert_eq!(phase.rejoin_reason(true, false), None, "{phase:?}");
    }
    assert!(LinkPhase::Rejoining.recovering() && LinkPhase::RadiusRecovering.recovering());
    assert!(!LinkPhase::Lost.recovering() && !LinkPhase::Attached.recovering());
}
