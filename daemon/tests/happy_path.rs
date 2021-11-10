use crate::harness::bdk::dummy_partially_signed_transaction;
use crate::harness::mocks::oracle::dummy_announcement;
use crate::harness::mocks::wallet::build_party_params;
use crate::harness::start_both;
use anyhow::Context;
use daemon::{maker_cfd, monitor};
use daemon::model::cfd::{Cfd, CfdState, Order, Origin};
use daemon::model::{Price, Usd};
use daemon::tokio_ext::FutureExt;
use harness::bdk::dummy_tx_id;
use maia::secp256k1_zkp::schnorrsig;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::sync::{watch, MutexGuard};
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

mod harness;

#[tokio::test]
async fn taker_receives_order_from_maker_on_publication() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    assert!(is_next_none(&mut taker.order_feed).await);

    maker.publish_order(dummy_new_order()).await;

    let (published, received) = tokio::join!(
        next_some(&mut maker.order_feed),
        next_some(&mut taker.order_feed)
    );

    assert_is_same_order(&published, &received);
}

#[tokio::test]
async fn taker_takes_order_and_maker_rejects() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    // TODO: Why is this needed? For the cfd stream it is not needed
    is_next_none(&mut taker.order_feed).await;

    maker.publish_order(dummy_new_order()).await;

    let (_, received) = next_order(&mut maker.order_feed, &mut taker.order_feed).await;

    taker.take_order(received.clone(), Usd::new(dec!(10))).await;

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    assert_is_same_order(&taker_cfd.order, &received);
    assert_is_same_order(&maker_cfd.order, &received);
    assert!(matches!(
        taker_cfd.state,
        CfdState::OutgoingOrderRequest { .. }
    ));
    assert!(matches!(
        maker_cfd.state,
        CfdState::IncomingOrderRequest { .. }
    ));

    maker.reject_take_request(received.clone()).await;

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    // TODO: More elaborate Cfd assertions
    assert_is_same_order(&taker_cfd.order, &received);
    assert_is_same_order(&maker_cfd.order, &received);
    assert!(matches!(taker_cfd.state, CfdState::Rejected { .. }));
    assert!(matches!(maker_cfd.state, CfdState::Rejected { .. }));
}

#[tokio::test]
async fn taker_proposes_rollover_and_maker_accepts_rollover() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    maker
        .mocks
        .oracle()
        .await
        .expect_get_announcement()
        .returning(|_| Some(dummy_announcement()));

    taker
        .mocks
        .oracle()
        .await
        .expect_get_announcement()
        .returning(|_| Some(dummy_announcement()));

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        maker
        .mocks
        .wallet()
        .await
        .expect_build_party_params()
        .returning(|msg| build_party_params(msg));

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        taker
        .mocks
        .wallet()
        .await
        .expect_build_party_params()
        .returning(|msg| build_party_params(msg));

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        maker
        .mocks
        .oracle()
        .await
        .expect_monitor_attestation()
        .return_const(());

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        taker
        .mocks
        .oracle()
        .await
        .expect_monitor_attestation()
        .return_const(());

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        maker
        .mocks
        .monitor()
        .await
        .expect_start_monitoring()
        .return_const(());

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
        taker
        .mocks
        .monitor()
        .await
        .expect_start_monitoring()
        .return_const(());


    is_next_none(&mut taker.order_feed).await;

    maker.publish_order(dummy_new_order()).await;

    let (_, received) = next_order(&mut maker.order_feed, &mut taker.order_feed).await;

    taker.take_order(received.clone(), Usd::new(dec!(5))).await;
    let (_, _) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;

    maker.accept_take_request(received.clone()).await;

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    // TODO: More elaborate Cfd assertions
    assert_eq!(taker_cfd.order.id, received.id);
    assert_eq!(maker_cfd.order.id, received.id);
    assert!(matches!(taker_cfd.state, CfdState::ContractSetup { .. }));
    assert!(matches!(maker_cfd.state, CfdState::ContractSetup { .. }));

    mock_wallet_sign_and_broadcast(&mut maker.mocks.wallet().await);
    mock_wallet_sign_and_broadcast(&mut taker.mocks.wallet().await);

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    // TODO: More elaborate Cfd assertions
    assert_eq!(taker_cfd.order.id, received.id);
    assert_eq!(maker_cfd.order.id, received.id);
    assert!(matches!(taker_cfd.state, CfdState::PendingOpen { .. }));
    assert!(matches!(maker_cfd.state, CfdState::PendingOpen { .. }));

    maker.cfd_actor_addr.send(monitor::Event::LockFinality(maker_cfd.order.id)).await.unwrap();
    taker.cfd_actor_addr.send(monitor::Event::LockFinality(taker_cfd.order.id)).await.unwrap();

    dbg!("sent lock finality event to cfd actor");

    let (taker_cfd_open_state, maker_cfd_open_state) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;

    dbg!("trying to retrieve the cfd in open state");

    assert!(matches!(taker_cfd_open_state.state, CfdState::Open { .. }));
    assert!(matches!(maker_cfd_open_state.state, CfdState::Open { .. }));

    dbg!("retrieved what we beleive to be the cfd in open state");

    taker.propose_rollover(taker_cfd.order.id).await;
    dbg!("proposed roll over");
    maker.accept_rollover(maker_cfd.order.clone()).await;

    dbg!("accepted roll over");
    //
    // let (taker_cfd_rolled_over, maker_cfd_rolled_over) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    //
    // assert!(matches!(taker_cfd_rolled_over.state, CfdState::Open{ .. }));
    // assert!(matches!(maker_cfd_rolled_over.state, CfdState::Open { .. }));
    //
    // assert_ne!(taker_cfd_rolled_over.state.get_transition_timestamp(), taker_cfd_rolled_over.state.get_transition_timestamp());

    // punish key only acquired after rollover
    // assert that tx commit has changed

    // let taker_order = next_some(&mut taker.order_feed).await;
    // let maker_order= next_some(&mut maker.order_feed).await;

    // assert_is_same_order(&taker_order, &received);
    // assert_is_same_order(&maker_order, &received);


}


// Helper function setting up a "happy path" wallet mock
fn mock_wallet_sign_and_broadcast(wallet: &mut MutexGuard<'_, harness::mocks::wallet::MockWallet>) {
    let mut seq = mockall::Sequence::new();
    wallet
        .expect_sign()
        .times(1)
        .returning(|_| Ok(dummy_partially_signed_transaction()))
        .in_sequence(&mut seq);
    wallet
        .expect_broadcast()
        .times(1)
        .returning(|_| Ok(dummy_tx_id()))
        .in_sequence(&mut seq);
}

#[tokio::test]
// #[cfg_attr(not(feature = "expensive_tests"), ignore)]
async fn taker_takes_order_and_maker_accepts_and_contract_setup() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    is_next_none(&mut taker.order_feed).await;

    maker.publish_order(dummy_new_order()).await;

    let (_, received) = next_order(&mut maker.order_feed, &mut taker.order_feed).await;

    taker.take_order(received.clone(), Usd::new(dec!(5))).await;
    let (_, _) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;

    maker
        .mocks
        .oracle()
        .await
        .expect_get_announcement()
        .returning(|_| Some(dummy_announcement()));

    taker
        .mocks
        .oracle()
        .await
        .expect_get_announcement()
        .returning(|_| Some(dummy_announcement()));

    maker.accept_take_request(received.clone()).await;

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
    maker
        .mocks
        .wallet()
        .await
        .expect_build_party_params()
        .times(1)
        .returning(|msg| build_party_params(msg));

    #[allow(clippy::redundant_closure)] // clippy is in the wrong here
    taker
        .mocks
        .wallet()
        .await
        .expect_build_party_params()
        .times(1)
        .returning(|msg| build_party_params(msg));

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    // TODO: More elaborate Cfd assertions
    assert_eq!(taker_cfd.order.id, received.id);
    assert_eq!(maker_cfd.order.id, received.id);
    assert!(matches!(taker_cfd.state, CfdState::ContractSetup { .. }));
    assert!(matches!(maker_cfd.state, CfdState::ContractSetup { .. }));

    mock_wallet_sign_and_broadcast(&mut maker.mocks.wallet().await);
    mock_wallet_sign_and_broadcast(&mut taker.mocks.wallet().await);

    let (taker_cfd, maker_cfd) = next_cfd(&mut taker.cfd_feed, &mut maker.cfd_feed).await;
    // TODO: More elaborate Cfd assertions
    assert_eq!(taker_cfd.order.id, received.id);
    assert_eq!(maker_cfd.order.id, received.id);
    assert!(matches!(taker_cfd.state, CfdState::PendingOpen { .. }));
    assert!(matches!(maker_cfd.state, CfdState::PendingOpen { .. }));
}

/// The order cannot be directly compared in tests as the origin is different,
/// therefore wrap the assertion macro in a code that unifies the 'Origin'
fn assert_is_same_order(a: &Order, b: &Order) {
    // Assume the same origin
    let mut a = a.clone();
    let mut b = b.clone();
    a.origin = Origin::Ours;
    b.origin = Origin::Ours;

    assert_eq!(a, b);
}

fn dummy_new_order() -> maker_cfd::NewOrder {
    maker_cfd::NewOrder {
        price: Price::new(dec!(50_000)).expect("unexpected failure"),
        min_quantity: Usd::new(dec!(5)),
        max_quantity: Usd::new(dec!(100)),
    }
}

/// Returns the first `Cfd` from both channels
///
/// Ensures that there is only one `Cfd` present in both channels.
async fn next_cfd(
    rx_a: &mut watch::Receiver<Vec<Cfd>>,
    rx_b: &mut watch::Receiver<Vec<Cfd>>,
) -> (Cfd, Cfd) {
    let (a, b) = tokio::join!(next(rx_a), next(rx_b));

    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);

    (a.first().unwrap().clone(), b.first().unwrap().clone())
}

async fn next_order(
    rx_a: &mut watch::Receiver<Option<Order>>,
    rx_b: &mut watch::Receiver<Option<Order>>,
) -> (Order, Order) {
    let (a, b) = tokio::join!(next_some(rx_a), next_some(rx_b));

    (a, b)
}

/// Returns the value if the next Option received on the stream is Some
///
/// Panics if None is received on the stream.
async fn next_some<T>(rx: &mut watch::Receiver<Option<T>>) -> T
where
    T: Clone,
{
    if let Some(value) = next(rx).await {
        value
    } else {
        panic!("Received None when Some was expected")
    }
}

/// Returns true if the next Option received on the stream is None
///
/// Returns false if Some is received.
async fn is_next_none<T>(rx: &mut watch::Receiver<Option<T>>) -> bool
where
    T: Clone,
{
    next(rx).await.is_none()
}

/// Returns watch channel value upon change
async fn next<T>(rx: &mut watch::Receiver<T>) -> T
where
    T: Clone,
{
    // TODO: Make timeout configurable, only contract setup can take up to 2 min on CI
    rx.changed()
        .timeout(Duration::from_secs(120))
        .await
        .context("Waiting for next element in channel is taking too long, aborting")
        .unwrap()
        .unwrap();
    rx.borrow().clone()
}

fn init_tracing() -> DefaultGuard {
    let filter = EnvFilter::from_default_env()
        // apply warning level globally
        .add_directive(format!("{}", LevelFilter::WARN).parse().unwrap())
        // log traces from test itself
        .add_directive(
            format!("happy_path={}", LevelFilter::DEBUG)
                .parse()
                .unwrap(),
        )
        .add_directive(format!("taker={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("maker={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("daemon={}", LevelFilter::DEBUG).parse().unwrap())
        .add_directive(format!("rocket={}", LevelFilter::WARN).parse().unwrap());

    let guard = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .set_default();

    tracing::info!("Running version: {}", env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT"));

    guard
}
