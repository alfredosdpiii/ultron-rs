//! The server's ledger of attachment demand per worker (daemon::workers `DemandLedger`): a detach the worker did not
//! confirm is not counted as applied. Counting it would leave the worker holding the demand with nothing on the server
//! tracking it, so the worker never retires while that server lives.

use daemon::workers::{AttachmentDemand, DemandError, DemandLedger};

#[test]
fn daemon_timed_out_detach_stays_owed_until_the_worker_confirms_it() {
    let mut ledger = DemandLedger::default();
    ledger.attach("a1");
    ledger.settle("a1", true, &Ok(()));
    assert!(ledger.is_attached("a1") && ledger.any_attached());

    // The client leaves; the detach times out.
    assert!(ledger.withdraw("a1"));
    ledger.settle("a1", false, &Err(DemandError::TimedOut));
    assert_eq!(ledger.state("a1"), Some(AttachmentDemand::Withdrawing));
    assert_eq!(ledger.owed(), ["a1"]);
    // Released: no longer routed, and no longer a reason to restart the worker.
    assert!(!ledger.is_attached("a1") && !ledger.any_attached());

    // The heartbeat sends it again; the worker applies it.
    ledger.settle("a1", false, &Ok(()));
    assert_eq!(ledger.state("a1"), None);
    assert!(ledger.owed().is_empty());
}

#[test]
fn daemon_demand_ledger_settles_attaches_and_refused_detaches() {
    let mut ledger = DemandLedger::default();
    // An attach the worker refused is dropped.
    ledger.attach("refused");
    ledger.settle("refused", true, &Err(DemandError::Failed("retiring".into())));
    assert_eq!(ledger.state("refused"), None);
    // An attach that timed out may be held: it is withdrawn.
    ledger.attach("lost");
    ledger.settle("lost", true, &Err(DemandError::TimedOut));
    assert_eq!(ledger.owed(), ["lost"]);
    // A detach the worker refused (it is retiring, or answers another server) or could not get is settled.
    ledger.settle("lost", false, &Err(DemandError::Failed("retiring".into())));
    assert!(ledger.owed().is_empty());
    // A late answer for an attachment attached again (a new one) or never known changes nothing.
    ledger.attach("b");
    ledger.settle("b", false, &Ok(()));
    assert!(ledger.is_attached("b"));
    ledger.settle("unknown", false, &Ok(()));
    assert!(!ledger.withdraw("unknown"));
}
