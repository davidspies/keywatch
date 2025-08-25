use std::sync::Arc;
use std::time::Duration;

use keywatch::{TryRecvError, Update, channel};

// 1. Re-add after Delete during cooldown
#[tokio::test]
async fn re_add_after_delete_during_cooldown() {
    let cooldown = Duration::from_millis(100);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Second add enters cooldown slot
    tx.send("k", Update::Add(2)).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await; // still cooling
    // Delete before cooldown expires
    tx.send("k", Update::Delete).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
    // Re-add before original cooldown expiry
    tx.send("k", Update::Add(3)).unwrap();
    // Wait until original expiry should have passed
    tokio::time::sleep(Duration::from_millis(80)).await; // total ~110ms > 100ms
    assert_eq!(rx.recv().await, Some(("k", Update::Add(3))));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 2. Replacement chain: only last value emerges after cooldown
#[tokio::test]
async fn replacement_chain_only_last_emerges() {
    let cooldown = Duration::from_millis(120);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    // Multiple replacements while in cooldown
    tx.send("k", Update::Add(2)).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send("k", Update::Add(3)).unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    tx.send("k", Update::Add(4)).unwrap();
    // Sleep past cooldown
    tokio::time::sleep(Duration::from_millis(130)).await;
    assert_eq!(rx.recv().await, Some(("k", Update::Add(4))));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 3. Many simultaneous matured cooldowns preserve original order
#[tokio::test]
async fn matured_order_preserved() {
    let cooldown = Duration::from_millis(80);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    // Initial adds interleaved
    for (k, v) in [("a", 1), ("b", 2), ("c", 3)] {
        tx.send(k, Update::Add(v)).unwrap();
    }
    // Receive them establishing cooldown entries in order a,b,c
    let mut first_round = vec![
        rx.recv().await.unwrap(),
        rx.recv().await.unwrap(),
        rx.recv().await.unwrap(),
    ];
    first_round.sort_by_key(|(k, _)| *k);
    assert_eq!(
        first_round,
        vec![
            ("a", Update::Add(1)),
            ("b", Update::Add(2)),
            ("c", Update::Add(3))
        ]
    );
    // Send replacement values in reverse order while cooling to ensure order isn't affected
    tx.send("c", Update::Add(30)).unwrap();
    tx.send("b", Update::Add(20)).unwrap();
    tx.send("a", Update::Add(10)).unwrap();
    // Wait for all cooldowns to mature
    tokio::time::sleep(Duration::from_millis(90)).await;
    // Collect matured adds, expecting in original order a,b,c
    let second_a = rx.recv().await.unwrap();
    let second_b = rx.recv().await.unwrap();
    let second_c = rx.recv().await.unwrap();
    assert_eq!(second_a, ("a", Update::Add(10)));
    assert_eq!(second_b, ("b", Update::Add(20)));
    assert_eq!(second_c, ("c", Update::Add(30)));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 4. Complex sequence Add -> recv -> Add -> Delete -> Add (within same cooldown) -> matured final Add
#[tokio::test]
async fn complex_sequence_add_delete_add() {
    let cooldown = Duration::from_millis(150);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    tx.send("k", Update::Add(2)).unwrap(); // replacement candidate
    tokio::time::sleep(Duration::from_millis(40)).await;
    tx.send("k", Update::Delete).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Delete)));
    // Add again still before original cooldown expiry
    tokio::time::sleep(Duration::from_millis(30)).await; // t=70ms
    tx.send("k", Update::Add(3)).unwrap();
    // Wait until >150ms from first add consumption
    tokio::time::sleep(Duration::from_millis(90)).await; // total ~160ms
    assert_eq!(rx.recv().await, Some(("k", Update::Add(3))));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 5. Concurrency: multiple tasks sending via Arc<Sender>
#[tokio::test]
async fn concurrent_senders_replacement_chain() {
    let cooldown = Duration::from_millis(120);
    let (sender, mut rx) = channel::<&'static str, i32>(cooldown);
    sender.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    let shared = Arc::new(sender);
    let mut handles = Vec::new();
    for v in 2..=5 {
        let s = shared.clone();
        handles.push(tokio::spawn(async move {
            s.send("k", Update::Add(v)).unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(130)).await;
    // Only last value (5) should emerge
    assert_eq!(rx.recv().await, Some(("k", Update::Add(5))));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 6. Zero cooldown coalescing when not received yet
#[tokio::test]
async fn zero_cooldown_coalesces_before_first_receive() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    tx.send("k", Update::Add(1)).unwrap();
    tx.send("k", Update::Add(2)).unwrap();
    tx.send("k", Update::Add(3)).unwrap();
    // Only last value should be delivered
    assert_eq!(rx.recv().await, Some(("k", Update::Add(3))));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 7. Zero cooldown sequential adds with immediate receives deliver each
#[tokio::test]
async fn zero_cooldown_sequential_adds_deliver_each() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::ZERO);
    for v in 1..=3 {
        tx.send("k", Update::Add(v)).unwrap();
        assert_eq!(rx.recv().await, Some(("k", Update::Add(v))));
    }
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 8. Clear no-op when channel empty
#[tokio::test]
async fn clear_noop_when_empty() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::from_millis(10));
    tx.clear().unwrap();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 9. Multiple clear calls before draining deletes don't duplicate events
#[tokio::test]
async fn multiple_clears_idempotent_pending_deletes() {
    let (tx, mut rx) = channel::<&'static str, i32>(Duration::from_millis(50));
    tx.send("a", Update::Add(1)).unwrap();
    tx.send("b", Update::Add(2)).unwrap();
    // Drain initial adds
    let mut first = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    first.sort_by_key(|(k, _)| *k);
    assert_eq!(first, vec![("a", Update::Add(1)), ("b", Update::Add(2))]);
    tx.clear().unwrap();
    tx.clear().unwrap(); // second clear shouldn't change resulting deletes
    let mut deletes = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    deletes.sort_by_key(|(k, _)| *k);
    assert_eq!(deletes, vec![("a", Update::Delete), ("b", Update::Delete)]);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

// 10. Drop sender while receiver waiting on cooldown causes pending value to be emitted immediately
#[tokio::test]
async fn drop_sender_while_waiting_on_cooldown_returns_value_then_none() {
    let cooldown = Duration::from_millis(200);
    let (tx, mut rx) = channel::<&'static str, i32>(cooldown);
    tx.send("k", Update::Add(1)).unwrap();
    assert_eq!(rx.recv().await, Some(("k", Update::Add(1))));
    tx.send("k", Update::Add(2)).unwrap(); // enters cooldown
    // move into spawned task
    let handle = tokio::spawn(async move { (rx.recv().await, rx) });
    // Drop sender before cooldown expires
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tx);
    let (result, mut rx) = handle.await.unwrap();
    assert_eq!(
        result,
        Some(("k", Update::Add(2))),
        "Expected cooled value to be delivered early after sender drop"
    );
    // Further recv after cooled value drained should now return None
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
}
