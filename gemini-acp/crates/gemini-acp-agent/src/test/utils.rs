use super::*;

#[tokio::test]
async fn pushable_preserves_order_and_closes_cleanly() {
    let queue = Pushable::new(); queue.push(1).await; queue.push(2).await; queue.close().await;
    assert_eq!(queue.next().await, Some(1)); assert_eq!(queue.next().await, Some(2)); assert_eq!(queue.next().await, None);
}

#[tokio::test]
async fn push_after_close_is_rejected() {
    let queue = Pushable::<u8>::new(); queue.close().await; assert!(!queue.push(1).await);
}

#[tokio::test]
async fn consumer_waits_until_producer_pushes() {
    let queue = Pushable::new(); let producer = queue.clone();
    let task = tokio::spawn(async move { tokio::task::yield_now().await; producer.push("ready").await });
    assert_eq!(queue.next().await, Some("ready")); assert!(task.await.unwrap());
}
