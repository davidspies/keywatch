use std::collections::HashSet;
use std::time::{Duration, Instant};

use keywatch::{TryRecvError, Update};

#[cfg(feature = "parking_lot")]
use keywatch::parking_lot::{channel, channel_with_starting_keys};
#[cfg(not(feature = "parking_lot"))]
use keywatch::{channel, channel_with_starting_keys};

#[tokio::test]
async fn add_and_recv_basic() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    tx.send("a", Update::Add(1)).unwrap();
    assert_eq!(rx.try_recv().unwrap(), ("a", Update::Add(1)));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn delete_updates_state() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    tx.send("k", Update::Add(10)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(10))));
    tx.send("k", Update::Delete).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
}

#[tokio::test]
async fn clear_generates_deletes_for_existing_keys() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    tx.send("a", Update::Add(1)).unwrap();
    tx.send("b", Update::Add(2)).unwrap();
    // Drain adds
    let mut seen = Vec::new();
    seen.push(rx.recv().await.unwrap());
    seen.push(rx.recv().await.unwrap());
    seen.sort_by_key(|(k, _)| *k);
    assert_eq!(seen, vec![("a", Update::Add(1)), ("b", Update::Add(2))]);
    tx.clear().unwrap();
    // Expect two deletes (order doesn't matter)
    let mut deletes = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    deletes.sort_by_key(|(k, _)| *k);
    assert_eq!(deletes, vec![("a", Update::Delete), ("b", Update::Delete)]);
}

#[tokio::test]
async fn cooldown_coalesces_updates_until_sent() {
    // cooldown > 0 means after Add is consumed, further Add before cooldown expiry is queued and
    // delivered after cooldown unless a Delete occurs
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::from_millis(50));
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Immediately send new value; should NOT be received until cooldown expires
    tx.send("k", Update::Add(2)).unwrap();
    // try_recv should be empty right now
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    // After sleeping past cooldown we should see the coalesced Add(2)
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(rx.recv().await, Some(("k", Update::Add(2))));
}

#[tokio::test]
async fn delete_during_cooldown_supersedes_pending_add() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::from_millis(50));
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    tx.send("k", Update::Add(2)).unwrap();
    // Delete before cooldown ends
    tx.send("k", Update::Delete).unwrap();
    // Sleep long enough that if Add were pending it would appear
    tokio::time::sleep(Duration::from_millis(60)).await;
    // We expect Delete, not Add(2)
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
    // No further events
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn disconnect_behavior() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    drop(tx);
    // After dropping sender, try_recv should return Disconnected because dropped flag is set in
    // Sender::drop
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
}

#[tokio::test]
async fn add_then_delete_before_receive_results_in_no_event() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    tx.send("k", Update::Add(1)).unwrap();
    tx.send("k", Update::Delete).unwrap();
    // The Add should have been removed by the Delete before receiver saw it; no Delete event is
    // queued either.
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    // Ensure nothing arrives later within a short window.
    tokio::select! {
        v = rx.recv() => panic!("unexpected event: {:?}", v),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
}

#[tokio::test]
async fn delete_then_add_before_receive_results_in_add_only() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    // Establish existing key so Delete has something meaningful to remove.
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Now queue a Delete followed immediately by an Add before receiver fetches either.
    tx.send("k", Update::Delete).unwrap();
    tx.send("k", Update::Add(42)).unwrap();
    // Expect only the Add(42); the intermediate Delete is superseded because it was never observed.
    assert_eq!(rx.try_recv().unwrap(), ("k", Update::Add(42)));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    tokio::select! {
        v = rx.recv() => panic!("unexpected extra event: {:?}", v),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
}

#[tokio::test]
async fn delete_twice_before_receive_results_in_single_delete() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    // Establish key so it is tracked by receiver.
    tx.send("k", Update::Add(7)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(7))));
    // Queue two deletes back-to-back before receiver pulls.
    tx.send("k", Update::Delete).unwrap();
    tx.send("k", Update::Delete).unwrap();
    // Only one Delete event should be observed.
    assert_eq!(rx.try_recv().unwrap(), ("k", Update::Delete));
    // Nothing else.
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    tokio::select! {
        v = rx.recv() => panic!("unexpected extra event after double delete: {:?}", v),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
}

#[tokio::test]
async fn add_second_then_delete_before_second_seen_results_in_delete() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    // First add
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Second add followed immediately by delete before receiver pulls again
    tx.send("k", Update::Add(2)).unwrap();
    tx.send("k", Update::Delete).unwrap();
    // Receiver should observe a Delete (not the intermediate Add(2))
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
    // And nothing further
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn clear_discards_cooldown_value_and_emits_delete() {
    let cooldown = Duration::from_millis(100);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    // First add and receive
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Second add goes into cooldown slot (not yet visible)
    tx.send("k", Update::Add(2)).unwrap();
    // Clear before cooldown expires
    // ensure some time passes but less than cooldown
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.clear().unwrap();
    // We should receive a Delete (not Add(2))
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
    // After cooldown would have expired, no Add should surface
    tokio::time::sleep(cooldown).await;
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn clear_mixed_after_second_send_before_other_matures() {
    // Scenario: A and B added and received. Second send for A happens while both still cooling.
    // Then we wait until only A has matured but B has not, and call clear. We expect only Deletes.
    let cooldown = Duration::from_millis(100);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);

    // Initial adds
    tx.send("A", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("A", Update::Add(1))));
    tokio::time::sleep(Duration::from_millis(20)).await; // stagger start times
    tx.send("B", Update::Add(10)).unwrap();
    assert_eq!(rx.recv().await, Some(("B", Update::Add(10))));

    // While both cooling, send an updated value for A (replaces cooldown slot value, keeps original expiry)
    tokio::time::sleep(Duration::from_millis(50)).await; // t ≈ 70ms since A inserted, 50ms since B
    tx.send("A", Update::Add(2)).unwrap();

    // Wait until A matured (~100ms) but before B (~120ms)
    tokio::time::sleep(Duration::from_millis(35)).await; // total ≈ 105ms; A matured, B not

    tx.clear().unwrap();

    // Expect Deletes for both A and B (order agnostic)
    let mut deletes = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    deletes.sort_by_key(|(k, _)| *k);
    assert_eq!(deletes, vec![("A", Update::Delete), ("B", Update::Delete)]);

    // No further events
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn recv_waits_for_cooldown_expiry() {
    // Ensure that recv() actually awaits the cooldown future (not a notification) for a matured
    // cooldown key.
    let cooldown = Duration::from_millis(120);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);

    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));

    // Second add enters cooldown
    tx.send("k", Update::Add(2)).unwrap();

    // Immediately call recv; it should block until cooldown matures.
    let start = Instant::now();
    let received = rx.recv().await;
    let elapsed = start.elapsed();

    assert_eq!(received, Some(("k", Update::Add(2))));
    // Allow a small tolerance for timer jitter.
    assert!(
        elapsed >= cooldown - Duration::from_millis(25),
        "recv returned too early: {:?} < {:?}",
        elapsed,
        cooldown
    );
}

#[tokio::test]
async fn recv_wakes_on_notification() {
    // Long cooldown; we expect recv to complete well before it if woken by a send notification.
    let cooldown = Duration::from_secs(2);
    let (tx, rx) = channel::<&'static str, i32>(cooldown);
    let start = Instant::now();
    let handle = tokio::spawn(async move {
        let mut rx = rx;
        rx.recv().await
    });
    // Give the task a brief moment to park in recv loop.
    tokio::time::sleep(Duration::from_millis(30)).await;
    tx.send("k", Update::Add(1)).unwrap();
    let out = handle.await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(out, Some(("k", Update::Add(1))));
    assert!(
        elapsed < cooldown / 4,
        "recv waited too long: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn recv_wakes_on_clear() {
    // Scenario: receiver is idle (no pending events), a key exists (so clear will generate a Delete),
    // and clear() is called while recv() is awaiting. recv() should wake promptly to deliver Delete.
    use std::time::Instant;
    let cooldown = Duration::from_secs(5); // long so we rely on notification, not cooldown expiry
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1)))); // drain initial add

    let start = Instant::now();
    let recv_fut = tokio::spawn(async move { rx.recv().await });
    // Give task time to park
    tokio::time::sleep(Duration::from_millis(40)).await;
    tx.clear().unwrap();
    let out = recv_fut.await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(out, Some(("k", Update::Delete)));
    // Should wake well before cooldown window.
    assert!(
        elapsed < cooldown / 10,
        "recv waited too long for clear notification: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn send_fails_after_receiver_drop() {
    let (tx, rx) = channel::<&'static str, i32>(Duration::ZERO);
    drop(rx); // trigger dropped flag
    let err = tx
        .send("k", Update::Add(42))
        .err()
        .expect("expected send error");
    match err.0 {
        Update::Add(v) => assert_eq!(v, 42),
        other => panic!("unexpected update in error: {:?}", other),
    }
}

#[tokio::test]
async fn clear_fails_after_receiver_drop() {
    let (tx, rx) = channel::<&'static str, i32>(Duration::ZERO);
    drop(rx);
    let err = tx.clear().err().expect("expected clear error");
    match err.0 {
        Update::Delete => {}
        other => panic!("unexpected update in clear error: {:?}", other),
    }
}

#[tokio::test]
async fn starting_key_delete_emitted_unknown_key_delete_suppressed() {
    // starting key "known" should produce Delete; unknown should not.
    let (tx, mut rx) = channel_with_starting_keys::<&'static str, i32>(
        HashSet::from_iter(["known"]),
        Duration::ZERO,
    );
    // Delete for starting key should appear.
    tx.send("known", Update::Delete).unwrap();
    assert_eq!(rx.try_recv().unwrap(), ("known", Update::Delete));
    // Delete for unknown key should be suppressed.
    tx.send("unknown", Update::Delete).unwrap();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn dropped_sender_ignores_cooldown_for_pending_value() {
    // After sender dropped, a value sitting in cooldown becomes immediately available via try_recv.
    let cooldown = Duration::from_millis(150);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Replacement enters cooldown window.
    tx.send("k", Update::Add(2)).unwrap();
    // Not yet matured.
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    // Drop sender; cooldown should now be ignored and pending value emitted.
    drop(tx);
    assert_eq!(rx.try_recv().unwrap(), ("k", Update::Add(2)));
    // Subsequent try_recv should report Disconnected (channel closed, no more values).
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
}

#[tokio::test]
async fn dropped_sender_releases_initial_pending_add() {
    // An Add not yet received becomes available immediately after sender drop.
    let cooldown = Duration::from_millis(200);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(10)).unwrap();
    // Drop sender before receiver consumes first Add.
    drop(tx);
    // try_recv should now succeed even though we never called recv() before.
    assert_eq!(rx.try_recv().unwrap(), ("k", Update::Add(10)));
    // Further calls report Disconnected.
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
}

#[tokio::test]
async fn sender_closed_waits_for_receiver_drop() {
    // Verify that Sender::closed() does not complete while the receiver is alive, and completes
    // promptly once the receiver is dropped.
    let (tx, rx) = channel::<&'static str, i32>(Duration::from_millis(50));
    let handle = tokio::spawn(async move {
        tx.closed().await;
    });

    // Allow some time; closed() should still be pending because channel not dropped yet.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !handle.is_finished(),
        "Sender::closed() returned before channel was closed"
    );

    // Drop receiver to close the channel; closed() future should now complete.
    drop(rx);

    // Await the handle with a timeout to ensure it completes promptly.
    tokio::time::timeout(Duration::from_millis(100), handle)
        .await
        .expect("Sender::closed() did not complete after receiver drop")
        .unwrap();
}
