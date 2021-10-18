use crate::cfd_actors::{self, append_cfd_state, insert_cfd};
use crate::db::{insert_order, load_cfd_by_order_id, load_order_by_id};
use crate::model::cfd::{
    Cfd, CfdState, CfdStateChangeEvent, CfdStateCommon, CollaborativeSettlement, Dlc, Order,
    OrderId, Origin, Role, RollOverProposal, SettlementKind, SettlementProposal, UpdateCfdProposal,
    UpdateCfdProposals,
};
use crate::model::{BitMexPriceEventId, Usd};
use crate::monitor::{self, MonitorParams};
use crate::wallet::Wallet;
use crate::wire::{MakerToTaker, RollOverMsg, SetupMsg};
use crate::{log_error, oracle, send_to_socket, setup_contract, wire};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use bdk::bitcoin::secp256k1::schnorrsig;
use futures::channel::mpsc;
use futures::{future, SinkExt};
use std::collections::HashMap;
use std::time::SystemTime;
use tokio::sync::watch;
use xtra::prelude::*;
use xtra::KeepRunning;

pub struct TakeOffer {
    pub order_id: OrderId,
    pub quantity: Usd,
}

pub enum CfdAction {
    ProposeSettlement {
        order_id: OrderId,
        current_price: Usd,
    },
    ProposeRollOver {
        order_id: OrderId,
    },
    Commit {
        order_id: OrderId,
    },
}

pub struct MakerStreamMessage {
    pub item: Result<wire::MakerToTaker>,
}

pub struct CfdSetupCompleted {
    pub order_id: OrderId,
    pub dlc: Result<Dlc>,
}

pub struct CfdRollOverCompleted {
    pub order_id: OrderId,
    pub dlc: Result<Dlc>,
}

enum SetupState {
    Active {
        sender: mpsc::UnboundedSender<SetupMsg>,
    },
    None,
}

enum RollOverState {
    Active {
        sender: mpsc::UnboundedSender<RollOverMsg>,
    },
    None,
}

pub struct Actor {
    db: sqlx::SqlitePool,
    wallet: Wallet,
    oracle_pk: schnorrsig::PublicKey,
    cfd_feed_actor_inbox: watch::Sender<Vec<Cfd>>,
    order_feed_actor_inbox: watch::Sender<Option<Order>>,
    update_cfd_feed_sender: watch::Sender<UpdateCfdProposals>,
    send_to_maker: Address<send_to_socket::Actor<wire::TakerToMaker>>,
    monitor_actor: Address<monitor::Actor>,
    setup_state: SetupState,
    roll_over_state: RollOverState,
    oracle_actor: Address<oracle::Actor>,
    current_pending_proposals: UpdateCfdProposals,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlx::SqlitePool,
        wallet: Wallet,
        oracle_pk: schnorrsig::PublicKey,
        cfd_feed_actor_inbox: watch::Sender<Vec<Cfd>>,
        order_feed_actor_inbox: watch::Sender<Option<Order>>,
        update_cfd_feed_sender: watch::Sender<UpdateCfdProposals>,
        send_to_maker: Address<send_to_socket::Actor<wire::TakerToMaker>>,
        monitor_actor: Address<monitor::Actor>,
        oracle_actor: Address<oracle::Actor>,
    ) -> Self {
        Self {
            db,
            wallet,
            oracle_pk,
            cfd_feed_actor_inbox,
            order_feed_actor_inbox,
            update_cfd_feed_sender,
            send_to_maker,
            monitor_actor,
            setup_state: SetupState::None,
            roll_over_state: RollOverState::None,
            oracle_actor,
            current_pending_proposals: HashMap::new(),
        }
    }

    fn send_pending_update_proposals(&self) -> Result<()> {
        Ok(self
            .update_cfd_feed_sender
            .send(self.current_pending_proposals.clone())?)
    }

    /// Removes a proposal and updates the update cfd proposals' feed
    fn remove_pending_proposal(&mut self, order_id: &OrderId) -> Result<()> {
        if self.current_pending_proposals.remove(order_id).is_none() {
            anyhow::bail!("Could not find proposal with order id: {}", &order_id)
        }
        self.send_pending_update_proposals()?;
        Ok(())
    }

    fn get_settlement_proposal(&self, order_id: OrderId) -> Result<&SettlementProposal> {
        match self
            .current_pending_proposals
            .get(&order_id)
            .context("have a proposal that is about to be accepted")?
        {
            UpdateCfdProposal::Settlement { proposal, .. } => Ok(proposal),
            UpdateCfdProposal::RollOverProposal { .. } => {
                anyhow::bail!("did not expect a rollover proposal");
            }
        }
    }

    async fn handle_take_offer(&mut self, order_id: OrderId, quantity: Usd) -> Result<()> {
        let mut conn = self.db.acquire().await?;

        let current_order = load_order_by_id(order_id, &mut conn).await?;

        tracing::info!("Taking current order: {:?}", &current_order);

        let cfd = Cfd::new(
            current_order.clone(),
            quantity,
            CfdState::outgoing_order_request(),
        );

        insert_cfd(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        self.send_to_maker
            .do_send_async(wire::TakerToMaker::TakeOrder { order_id, quantity })
            .await?;

        Ok(())
    }

    async fn handle_propose_settlement(
        &mut self,
        order_id: OrderId,
        current_price: Usd,
    ) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        let cfd = load_cfd_by_order_id(order_id, &mut conn).await?;

        let proposal = cfd.calculate_settlement(current_price)?;

        if self
            .current_pending_proposals
            .contains_key(&proposal.order_id)
        {
            anyhow::bail!(
                "Settlement proposal for order id {} already present",
                order_id
            )
        }

        self.current_pending_proposals.insert(
            proposal.order_id,
            UpdateCfdProposal::Settlement {
                proposal: proposal.clone(),
                direction: SettlementKind::Outgoing,
            },
        );
        self.send_pending_update_proposals()?;

        self.send_to_maker
            .do_send_async(wire::TakerToMaker::ProposeSettlement {
                order_id: proposal.order_id,
                timestamp: proposal.timestamp,
                taker: proposal.taker,
                maker: proposal.maker,
                price: proposal.price,
            })
            .await?;
        Ok(())
    }

    async fn handle_propose_roll_over(&mut self, order_id: OrderId) -> Result<()> {
        if self.current_pending_proposals.contains_key(&order_id) {
            anyhow::bail!("An update for order id {} is already in progress", order_id)
        }

        let proposal = RollOverProposal {
            order_id,
            timestamp: SystemTime::now(),
        };

        self.current_pending_proposals.insert(
            proposal.order_id,
            UpdateCfdProposal::RollOverProposal {
                proposal: proposal.clone(),
                direction: SettlementKind::Outgoing,
            },
        );
        self.send_pending_update_proposals()?;

        self.send_to_maker
            .do_send_async(wire::TakerToMaker::ProposeRollOver {
                order_id: proposal.order_id,
                timestamp: proposal.timestamp,
            })
            .await?;
        Ok(())
    }

    async fn handle_new_order(&mut self, order: Option<Order>) -> Result<()> {
        match order {
            Some(mut order) => {
                order.origin = Origin::Theirs;

                self.oracle_actor
                    .do_send_async(oracle::FetchAnnouncement(order.oracle_event_id))
                    .await?;

                let mut conn = self.db.acquire().await?;
                insert_order(&order, &mut conn).await?;
                self.order_feed_actor_inbox.send(Some(order))?;
            }
            None => {
                self.order_feed_actor_inbox.send(None)?;
            }
        }
        Ok(())
    }

    async fn handle_order_accepted(
        &mut self,
        order_id: OrderId,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::info!(%order_id, "Order got accepted");

        let (sender, receiver) = mpsc::unbounded();

        if let SetupState::Active { .. } = self.setup_state {
            anyhow::bail!("Already setting up a contract!")
        }

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::contract_setup();

        append_cfd_state(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        let offer_announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(cfd.order.oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", cfd.order.oracle_event_id))?;

        self.oracle_actor
            .do_send_async(oracle::MonitorAttestation {
                event_id: offer_announcement.id,
            })
            .await?;

        let contract_future = setup_contract::new(
            self.send_to_maker
                .clone()
                .into_sink()
                .with(|msg| future::ok(wire::TakerToMaker::Protocol(msg))),
            receiver,
            (self.oracle_pk, offer_announcement),
            cfd,
            self.wallet.clone(),
            Role::Taker,
        );

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        tokio::spawn(async move {
            let dlc = contract_future.await;

            this.do_send_async(CfdSetupCompleted { order_id, dlc })
                .await
        });

        self.setup_state = SetupState::Active { sender };

        Ok(())
    }

    async fn handle_order_rejected(&mut self, order_id: OrderId) -> Result<()> {
        tracing::debug!(%order_id, "Order rejected");

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::rejected();

        append_cfd_state(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        Ok(())
    }

    async fn handle_settlement_accepted(
        &mut self,
        order_id: OrderId,
        _ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::info!(%order_id, "Settlement proposal got accepted");

        let mut conn = self.db.acquire().await?;

        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        let dlc = cfd.open_dlc().context("CFD was in wrong state")?;

        let proposal = self.get_settlement_proposal(order_id)?;
        let (tx, sig_taker) = dlc.close_transaction(proposal)?;

        self.send_to_maker
            .do_send_async(wire::TakerToMaker::InitiateSettlement {
                order_id,
                sig_taker,
            })
            .await?;

        cfd.handle(CfdStateChangeEvent::ProposalSigned(
            CollaborativeSettlement::new(
                tx.clone(),
                dlc.script_pubkey_for(cfd.role()),
                proposal.price,
            ),
        ))?;
        append_cfd_state(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        self.remove_pending_proposal(&order_id)?;

        self.monitor_actor
            .do_send_async(monitor::CollaborativeSettlement {
                order_id,
                tx: (tx.txid(), dlc.script_pubkey_for(Role::Taker)),
            })
            .await?;

        Ok(())
    }

    async fn handle_roll_over_accepted(
        &mut self,
        order_id: OrderId,
        oracle_event_id: BitMexPriceEventId,
        ctx: &mut Context<Self>,
    ) -> Result<()> {
        tracing::info!(%order_id, "Roll; over request got accepted");

        let (sender, receiver) = mpsc::unbounded();

        if let RollOverState::Active { .. } = self.roll_over_state {
            anyhow::bail!("Already rolling over a contract!")
        }

        let mut conn = self.db.acquire().await?;

        let cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        let dlc = cfd.open_dlc().context("CFD was in wrong state")?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", oracle_event_id))?;

        let contract_future = setup_contract::roll_over(
            self.send_to_maker
                .clone()
                .into_sink()
                .with(|msg| future::ok(wire::TakerToMaker::RollOverProtocol(msg))),
            receiver,
            (self.oracle_pk, announcement),
            cfd,
            Role::Taker,
            dlc,
        );

        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        self.roll_over_state = RollOverState::Active { sender };

        tokio::spawn(async move {
            let dlc = contract_future.await;

            this.do_send_async(CfdRollOverCompleted { order_id, dlc })
                .await
        });

        self.remove_pending_proposal(&order_id)
            .context("Could not remove accepted roll over")?;
        Ok(())
    }

    async fn handle_settlement_rejected(&mut self, order_id: OrderId) -> Result<()> {
        tracing::info!(%order_id, "Settlement proposal got rejected");

        self.remove_pending_proposal(&order_id)?;

        Ok(())
    }

    async fn handle_roll_over_rejected(&mut self, order_id: OrderId) -> Result<()> {
        tracing::debug!(%order_id, "Roll over request rejected");
        // TODO: tell UI that roll over was rejected

        // this is not too bad as we are still monitoring for the CFD to expiry
        // the taker can just try to ask again :)

        Ok(())
    }

    async fn handle_inc_protocol_msg(&mut self, msg: SetupMsg) -> Result<()> {
        match &mut self.setup_state {
            SetupState::Active { sender } => {
                sender.send(msg).await?;
            }
            SetupState::None => {
                anyhow::bail!("Received setup message without an active contract setup")
            }
        }

        Ok(())
    }

    async fn handle_inc_roll_over_msg(&mut self, msg: RollOverMsg) -> Result<()> {
        match &mut self.roll_over_state {
            RollOverState::Active { sender } => {
                sender.send(msg).await?;
            }
            RollOverState::None => {
                anyhow::bail!("Received message without an active roll_over setup")
            }
        }

        Ok(())
    }

    async fn handle_cfd_setup_completed(
        &mut self,
        order_id: OrderId,
        dlc: Result<Dlc>,
    ) -> Result<()> {
        self.setup_state = SetupState::None;
        let dlc = dlc.context("Failed to setup contract with maker")?;

        tracing::info!("Setup complete, publishing on chain now");

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::PendingOpen {
            common: CfdStateCommon::default(),
            dlc: dlc.clone(),
            attestation: None,
        };

        append_cfd_state(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        let txid = self
            .wallet
            .try_broadcast_transaction(dlc.lock.0.clone())
            .await?;

        tracing::info!("Lock transaction published with txid {}", txid);

        self.monitor_actor
            .do_send_async(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::from_dlc_and_timelocks(dlc, cfd.refund_timelock_in_blocks()),
            })
            .await?;

        self.oracle_actor
            .do_send_async(oracle::MonitorAttestation {
                event_id: cfd.order.oracle_event_id,
            })
            .await?;

        Ok(())
    }

    async fn handle_cfd_roll_over_completed(
        &mut self,
        order_id: OrderId,
        dlc: Result<Dlc>,
    ) -> Result<()> {
        let dlc = dlc.context("Failed to roll over contract with maker")?;
        self.roll_over_state = RollOverState::None;

        let mut conn = self.db.acquire().await?;
        let mut cfd = load_cfd_by_order_id(order_id, &mut conn).await?;
        cfd.state = CfdState::Open {
            common: CfdStateCommon::default(),
            dlc: dlc.clone(),
            attestation: None,
            collaborative_close: None,
        };

        append_cfd_state(&cfd, &mut conn, &self.cfd_feed_actor_inbox).await?;

        self.monitor_actor
            .do_send_async(monitor::StartMonitoring {
                id: order_id,
                params: MonitorParams::from_dlc_and_timelocks(dlc, cfd.refund_timelock_in_blocks()),
            })
            .await?;

        Ok(())
    }

    async fn handle_monitoring_event(&mut self, event: monitor::Event) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_monitoring_event(
            event,
            &mut conn,
            &self.wallet,
            &self.cfd_feed_actor_inbox,
        )
        .await?;
        Ok(())
    }

    async fn handle_commit(&mut self, order_id: OrderId) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_commit(
            order_id,
            &mut conn,
            &self.wallet,
            &self.cfd_feed_actor_inbox,
        )
        .await?;
        Ok(())
    }

    async fn handle_oracle_attestation(&mut self, attestation: oracle::Attestation) -> Result<()> {
        let mut conn = self.db.acquire().await?;
        cfd_actors::handle_oracle_attestation(
            attestation,
            &mut conn,
            &self.wallet,
            &self.cfd_feed_actor_inbox,
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl Handler<TakeOffer> for Actor {
    async fn handle(&mut self, msg: TakeOffer, _ctx: &mut Context<Self>) {
        log_error!(self.handle_take_offer(msg.order_id, msg.quantity));
    }
}

#[async_trait]
impl Handler<CfdAction> for Actor {
    async fn handle(&mut self, msg: CfdAction, _ctx: &mut Context<Self>) {
        use CfdAction::*;

        if let Err(e) = match msg {
            Commit { order_id } => self.handle_commit(order_id).await,
            ProposeSettlement {
                order_id,
                current_price,
            } => {
                self.handle_propose_settlement(order_id, current_price)
                    .await
            }
            ProposeRollOver { order_id } => self.handle_propose_roll_over(order_id).await,
        } {
            tracing::error!("Message handler failed: {:#}", e);
        }
    }
}

#[async_trait]
impl Handler<MakerStreamMessage> for Actor {
    async fn handle(
        &mut self,
        message: MakerStreamMessage,
        ctx: &mut Context<Self>,
    ) -> KeepRunning {
        let msg = match message.item {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("Error while receiving message from maker: {:#}", e);
                return KeepRunning::Yes;
            }
        };

        match msg {
            wire::MakerToTaker::CurrentOrder(current_order) => {
                log_error!(self.handle_new_order(current_order))
            }
            wire::MakerToTaker::ConfirmOrder(order_id) => {
                log_error!(self.handle_order_accepted(order_id, ctx))
            }
            wire::MakerToTaker::RejectOrder(order_id) => {
                log_error!(self.handle_order_rejected(order_id))
            }
            wire::MakerToTaker::ConfirmSettlement(order_id) => {
                log_error!(self.handle_settlement_accepted(order_id, ctx))
            }
            wire::MakerToTaker::RejectSettlement(order_id) => {
                log_error!(self.handle_settlement_rejected(order_id))
            }
            wire::MakerToTaker::InvalidOrderId(_) => todo!(),
            wire::MakerToTaker::Protocol(setup_msg) => {
                log_error!(self.handle_inc_protocol_msg(setup_msg))
            }
            wire::MakerToTaker::ConfirmRollOver {
                order_id,
                oracle_event_id,
            } => {
                log_error!(self.handle_roll_over_accepted(order_id, oracle_event_id, ctx))
            }
            wire::MakerToTaker::RejectRollOver(order_id) => {
                log_error!(self.handle_roll_over_rejected(order_id))
            }
            MakerToTaker::RollOverProtocol(roll_over_msg) => {
                log_error!(self.handle_inc_roll_over_msg(roll_over_msg))
            }
        }

        KeepRunning::Yes
    }
}

#[async_trait]
impl Handler<CfdSetupCompleted> for Actor {
    async fn handle(&mut self, msg: CfdSetupCompleted, _ctx: &mut Context<Self>) {
        log_error!(self.handle_cfd_setup_completed(msg.order_id, msg.dlc));
    }
}

#[async_trait]
impl Handler<CfdRollOverCompleted> for Actor {
    async fn handle(&mut self, msg: CfdRollOverCompleted, _ctx: &mut Context<Self>) {
        log_error!(self.handle_cfd_roll_over_completed(msg.order_id, msg.dlc));
    }
}

#[async_trait]
impl Handler<monitor::Event> for Actor {
    async fn handle(&mut self, msg: monitor::Event, _ctx: &mut Context<Self>) {
        log_error!(self.handle_monitoring_event(msg))
    }
}

#[async_trait]
impl Handler<oracle::Attestation> for Actor {
    async fn handle(&mut self, msg: oracle::Attestation, _ctx: &mut Context<Self>) {
        log_error!(self.handle_oracle_attestation(msg))
    }
}

impl Message for TakeOffer {
    type Result = ();
}

impl Message for CfdAction {
    type Result = ();
}

// this signature is a bit different because we use `Address::attach_stream`
impl Message for MakerStreamMessage {
    type Result = KeepRunning;
}

impl Message for CfdSetupCompleted {
    type Result = ();
}

impl Message for CfdRollOverCompleted {
    type Result = ();
}

impl xtra::Actor for Actor {}
