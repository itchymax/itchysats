use crate::model::cfd::{Cfd, CfdState};
use crate::model::BitMexPriceEventId;
use crate::{log_error, tokio_ext};
use anyhow::{Context, Result};
use async_trait::async_trait;
use cfd_protocol::secp256k1_zkp::{schnorrsig, SecretKey};
use rocket::time::{OffsetDateTime, Time};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ops::Add;
use time::ext::NumericalDuration;
use xtra::prelude::StrongMessageChannel;

pub struct Actor {
    announcements: HashMap<BitMexPriceEventId, (OffsetDateTime, Vec<schnorrsig::PublicKey>)>,
    pending_announcements: HashSet<BitMexPriceEventId>,
    pending_attestations: HashSet<BitMexPriceEventId>,
    attestation_channel: Box<dyn StrongMessageChannel<Attestation>>,
}

pub struct Sync;

/// Message used to tell the `oracle::Actor` to fetch an
/// `Announcement` from `olivia`.
///
/// The `Announcement` corresponds to the `OracleEventId` included in
/// the message.
#[derive(Debug, Clone)]
pub struct FetchAnnouncement(pub BitMexPriceEventId);

pub struct MonitorAttestation {
    pub event_id: BitMexPriceEventId,
}

/// Message used to request the `Announcement` from the
/// `oracle::Actor`'s local state.
///
/// The `Announcement` corresponds to the [`BitMexPriceEventId`] included in
/// the message.
#[derive(Debug, Clone)]
pub struct GetAnnouncement(pub BitMexPriceEventId);

// TODO: Split xtra::Message and API object
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(try_from = "olivia_api::Response")]
pub struct Attestation {
    pub id: BitMexPriceEventId,
    pub price: u64,
    pub scalars: Vec<SecretKey>,
}

/// A module-private message to allow parallelization of fetching announcements.
#[derive(Debug)]
struct NewAnnouncementFetched {
    id: BitMexPriceEventId,
    expected_outcome_time: OffsetDateTime,
    nonce_pks: Vec<schnorrsig::PublicKey>,
}

/// A module-private message to allow parallelization of fetching attestations.
#[derive(Debug)]
struct NewAttestationFetched {
    id: BitMexPriceEventId,
    attestation: Attestation,
}

impl Actor {
    pub fn new(
        cfds: Vec<Cfd>,
        attestation_channel: impl StrongMessageChannel<Attestation> + 'static,
    ) -> Self {
        let mut pending_attestations = HashSet::new();

        for cfd in cfds {
            match cfd.state.clone() {
                CfdState::PendingOpen { .. }
                | CfdState::Open { .. }
                | CfdState::PendingCommit { .. }
                | CfdState::OpenCommitted { .. }
                | CfdState::PendingCet { .. } =>
                {
                    pending_attestations.insert(cfd.order.oracle_event_id);
                }

                // Irrelevant for restart
                CfdState::OutgoingOrderRequest { .. }
                | CfdState::IncomingOrderRequest { .. }
                | CfdState::Accepted { .. }
                | CfdState::Rejected { .. }
                | CfdState::ContractSetup { .. }

                // Final states
                | CfdState::Closed { .. }
                | CfdState::MustRefund { .. }
                | CfdState::Refunded { .. }
                | CfdState::SetupFailed { .. } => ()
            }
        }

        Self {
            announcements: HashMap::new(),
            pending_announcements: HashSet::new(),
            pending_attestations,
            attestation_channel: Box::new(attestation_channel),
        }
    }
}

impl Actor {
    fn update_pending_announcements(&mut self, ctx: &mut xtra::Context<Self>) {
        for event_id in self.pending_announcements.iter().cloned() {
            let this = ctx.address().expect("self to be alive");

            tokio_ext::spawn_fallible(async move {
                let url = event_id.to_olivia_url();

                tracing::debug!("Fetching announcement for {}", event_id);

                let response = reqwest::get(url.clone())
                    .await
                    .with_context(|| format!("Failed to GET {}", url))?;

                if !response.status().is_success() {
                    anyhow::bail!("GET {} responded with {}", url, response.status());
                }

                let announcement = response
                    .json::<Announcement>()
                    .await
                    .context("Failed to deserialize as Announcement")?;

                this.send(NewAnnouncementFetched {
                    id: event_id,
                    nonce_pks: announcement.nonce_pks,
                    expected_outcome_time: announcement.expected_outcome_time,
                })
                .await?;

                Ok(())
            });
        }
    }
}

impl Actor {
    fn update_pending_attestations(&mut self, ctx: &mut xtra::Context<Self>) {
        for event_id in self.pending_attestations.iter().copied() {
            if !event_id.has_likely_occured() {
                tracing::trace!(
                    "Skipping {} because it likely hasn't occurred yet",
                    event_id
                );

                continue;
            }

            let this = ctx.address().expect("self to be alive");

            tokio_ext::spawn_fallible(async move {
                let url = event_id.to_olivia_url();

                tracing::debug!("Fetching attestation for {}", event_id);

                let response = reqwest::get(url.clone())
                    .await
                    .with_context(|| format!("Failed to GET {}", url))?;

                if !response.status().is_success() {
                    anyhow::bail!("GET {} responded with {}", url, response.status());
                }

                let attestation = response
                    .json::<Attestation>()
                    .await
                    .context("Failed to deserialize as Attestation")?;

                this.send(NewAttestationFetched {
                    id: event_id,
                    attestation,
                })
                .await?;

                Ok(())
            });
        }
    }

    async fn handle_new_attestation_fetched(
        &mut self,
        id: BitMexPriceEventId,
        attestation: Attestation,
    ) -> Result<()> {
        tracing::info!("Fetched new attestation for {}", id);

        let _ = self.attestation_channel.send(attestation).await;
        self.pending_attestations.remove(&id);

        Ok(())
    }
}

#[async_trait]
impl xtra::Handler<MonitorAttestation> for Actor {
    async fn handle(&mut self, msg: MonitorAttestation, _ctx: &mut xtra::Context<Self>) {
        if !self.pending_attestations.insert(msg.event_id) {
            tracing::trace!("Attestation {} already being monitored", msg.event_id);
        }
    }
}

#[async_trait]
impl xtra::Handler<FetchAnnouncement> for Actor {
    async fn handle(&mut self, msg: FetchAnnouncement, _ctx: &mut xtra::Context<Self>) {
        if !self.pending_announcements.insert(msg.0) {
            tracing::trace!("Announcement {} already being fetched", msg.0);
        }
    }
}

#[async_trait]
impl xtra::Handler<GetAnnouncement> for Actor {
    async fn handle(
        &mut self,
        msg: GetAnnouncement,
        _ctx: &mut xtra::Context<Self>,
    ) -> Option<Announcement> {
        let announcement =
            self.announcements
                .get_key_value(&msg.0)
                .map(|(id, (time, nonce_pks))| Announcement {
                    id: *id,
                    expected_outcome_time: *time,
                    nonce_pks: nonce_pks.clone(),
                });

        if announcement.is_none() {
            self.pending_announcements.insert(msg.0);
        }

        announcement
    }
}

#[async_trait]
impl xtra::Handler<NewAnnouncementFetched> for Actor {
    async fn handle(&mut self, msg: NewAnnouncementFetched, _ctx: &mut xtra::Context<Self>) {
        self.pending_announcements.remove(&msg.id);
        self.announcements
            .insert(msg.id, (msg.expected_outcome_time, msg.nonce_pks));
    }
}

#[async_trait]
impl xtra::Handler<NewAttestationFetched> for Actor {
    async fn handle(&mut self, msg: NewAttestationFetched, _ctx: &mut xtra::Context<Self>) {
        log_error!(self.handle_new_attestation_fetched(msg.id, msg.attestation));
    }
}

#[allow(dead_code)]
pub fn next_announcement_after(timestamp: OffsetDateTime) -> Result<BitMexPriceEventId> {
    let adjusted = ceil_to_next_hour(timestamp)?;

    Ok(BitMexPriceEventId::with_20_digits(adjusted))
}

fn ceil_to_next_hour(original: OffsetDateTime) -> Result<OffsetDateTime, anyhow::Error> {
    let timestamp = original.add(1.hours());
    let exact_hour = Time::from_hms(timestamp.hour(), 0, 0)
        .context("Could not adjust time for next announcement")?;
    let adjusted = timestamp.replace_time(exact_hour);

    Ok(adjusted)
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(try_from = "olivia_api::Response")]
pub struct Announcement {
    /// Identifier for an oracle event.
    ///
    /// Doubles up as the path of the URL for this event i.e.
    /// https://h00.ooo/{id}.
    pub id: BitMexPriceEventId,
    pub expected_outcome_time: OffsetDateTime,
    pub nonce_pks: Vec<schnorrsig::PublicKey>,
}

impl From<Announcement> for cfd_protocol::Announcement {
    fn from(announcement: Announcement) -> Self {
        cfd_protocol::Announcement {
            id: announcement.id.to_string(),
            nonce_pks: announcement.nonce_pks,
        }
    }
}

impl xtra::Actor for Actor {}

#[async_trait]
impl xtra::Handler<Sync> for Actor {
    async fn handle(&mut self, _: Sync, ctx: &mut xtra::Context<Self>) {
        self.update_pending_announcements(ctx);
        self.update_pending_attestations(ctx);
    }
}

impl xtra::Message for Sync {
    type Result = ();
}
impl xtra::Message for MonitorAttestation {
    type Result = ();
}

impl xtra::Message for FetchAnnouncement {
    type Result = ();
}

impl xtra::Message for GetAnnouncement {
    type Result = Option<Announcement>;
}

impl xtra::Message for Attestation {
    type Result = ();
}

impl xtra::Message for NewAnnouncementFetched {
    type Result = ();
}

impl xtra::Message for NewAttestationFetched {
    type Result = ();
}

mod olivia_api {
    use crate::model::BitMexPriceEventId;
    use anyhow::Context;
    use cfd_protocol::secp256k1_zkp::{schnorrsig, SecretKey};
    use std::convert::TryFrom;
    use time::OffsetDateTime;

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Response {
        announcement: Announcement,
        attestation: Option<Attestation>,
    }

    impl TryFrom<Response> for super::Announcement {
        type Error = serde_json::Error;

        fn try_from(response: Response) -> Result<Self, Self::Error> {
            // TODO: Verify signature here

            let data =
                serde_json::from_str::<AnnouncementData>(&response.announcement.oracle_event.data)?;

            Ok(Self {
                id: data.id,
                expected_outcome_time: data.expected_outcome_time,
                nonce_pks: data.schemes.olivia_v1.nonces,
            })
        }
    }

    impl TryFrom<Response> for super::Attestation {
        type Error = anyhow::Error;

        fn try_from(response: Response) -> Result<Self, Self::Error> {
            // TODO: Verify signature here

            let data =
                serde_json::from_str::<AnnouncementData>(&response.announcement.oracle_event.data)?;
            let attestation = response.attestation.context("attestation missing")?;

            Ok(Self {
                id: data.id,
                price: attestation.outcome.parse()?,
                scalars: attestation.schemes.olivia_v1.scalars,
            })
        }
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Announcement {
        oracle_event: OracleEvent,
        signature: String,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct OracleEvent {
        data: String,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct AnnouncementData {
        id: BitMexPriceEventId,
        #[serde(with = "timestamp")]
        expected_outcome_time: OffsetDateTime,
        schemes: Schemes,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Attestation {
        outcome: String,
        schemes: Schemes,
        #[serde(with = "timestamp")]
        time: OffsetDateTime,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct Schemes {
        #[serde(rename = "olivia-v1")]
        olivia_v1: OliviaV1,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct OliviaV1 {
        #[serde(default)]
        scalars: Vec<SecretKey>,
        #[serde(default)]
        nonces: Vec<schnorrsig::PublicKey>,
    }

    mod timestamp {
        use crate::olivia;
        use serde::de::Error as _;
        use serde::{Deserialize, Deserializer};
        use time::{OffsetDateTime, PrimitiveDateTime};

        pub fn deserialize<'a, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
        where
            D: Deserializer<'a>,
        {
            let string = String::deserialize(deserializer)?;
            let date_time = PrimitiveDateTime::parse(&string, &olivia::EVENT_TIME_FORMAT)
                .map_err(D::Error::custom)?;

            Ok(date_time.assume_utc())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::vec;

        use crate::model::BitMexPriceEventId;
        use crate::oracle;
        use time::macros::datetime;

        #[test]
        fn deserialize_announcement() {
            let json = r#"{"announcement":{"oracle_event":{"encoding":"json","data":"{\"id\":\"/x/BitMEX/BXBT/2021-10-04T22:00:00.price?n=20\",\"expected-outcome-time\":\"2021-10-04T22:00:00\",\"descriptor\":{\"type\":\"digit-decomposition\",\"is_signed\":false,\"n_digits\":20,\"unit\":null},\"schemes\":{\"olivia-v1\":{\"nonces\":[\"8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239\",\"77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e\",\"e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89\",\"ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a\",\"52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4\",\"e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40\",\"612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e\",\"e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90\",\"281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197\",\"3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033\",\"29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9\",\"20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523\",\"667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f\",\"224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1\",\"85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4\",\"96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b\",\"9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493\",\"b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6\",\"bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde\",\"20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe\"]},\"ecdsa-v1\":{}}}"},"signature":"743ed9900aba5a1ba3ba9d862628cdc5cca27974c40c4ab64618709021b3fbb13216a3efc733be260025da487ae9b63a8290d555bdc8da6324deff149fc7b110"},"attestation":{"outcome":"48935","schemes":{"olivia-v1":{"scalars":["1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484","72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6","051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e","847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7","5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998","368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee","9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999","fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738","44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086","587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d","010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955","5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520","06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd","fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe","0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068","c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f","86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269","fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db","41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d","ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"]},"ecdsa-v1":{"signature":"1d9a5e2336883cc6b440ff40e16ee44f8af2ba9313e46f1e4cd417f7dba7686279b0216e4b0b5fcf0c650dbad98fdefcf5ef16b49d63651a87f80caddd472384"}},"time":"2021-10-04T22:00:15"}}"#;

            let deserialized = serde_json::from_str::<oracle::Announcement>(json).unwrap();
            let expected = oracle::Announcement {
                id: BitMexPriceEventId::with_20_digits(datetime!(2021-10-04 22:00:00).assume_utc()),
                expected_outcome_time: datetime!(2021-10-04 22:00:00).assume_utc(),
                nonce_pks: vec![
                    "8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239"
                        .parse()
                        .unwrap(),
                    "77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e"
                        .parse()
                        .unwrap(),
                    "e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89"
                        .parse()
                        .unwrap(),
                    "ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a"
                        .parse()
                        .unwrap(),
                    "52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4"
                        .parse()
                        .unwrap(),
                    "e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40"
                        .parse()
                        .unwrap(),
                    "612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e"
                        .parse()
                        .unwrap(),
                    "e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90"
                        .parse()
                        .unwrap(),
                    "281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197"
                        .parse()
                        .unwrap(),
                    "3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033"
                        .parse()
                        .unwrap(),
                    "29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9"
                        .parse()
                        .unwrap(),
                    "20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523"
                        .parse()
                        .unwrap(),
                    "667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f"
                        .parse()
                        .unwrap(),
                    "224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1"
                        .parse()
                        .unwrap(),
                    "85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4"
                        .parse()
                        .unwrap(),
                    "96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b"
                        .parse()
                        .unwrap(),
                    "9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493"
                        .parse()
                        .unwrap(),
                    "b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6"
                        .parse()
                        .unwrap(),
                    "bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde"
                        .parse()
                        .unwrap(),
                    "20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe"
                        .parse()
                        .unwrap(),
                ],
            };

            assert_eq!(deserialized, expected)
        }

        #[test]
        fn deserialize_attestation() {
            let json = r#"{"announcement":{"oracle_event":{"encoding":"json","data":"{\"id\":\"/x/BitMEX/BXBT/2021-10-04T22:00:00.price?n=20\",\"expected-outcome-time\":\"2021-10-04T22:00:00\",\"descriptor\":{\"type\":\"digit-decomposition\",\"is_signed\":false,\"n_digits\":20,\"unit\":null},\"schemes\":{\"olivia-v1\":{\"nonces\":[\"8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239\",\"77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e\",\"e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89\",\"ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a\",\"52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4\",\"e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40\",\"612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e\",\"e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90\",\"281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197\",\"3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033\",\"29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9\",\"20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523\",\"667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f\",\"224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1\",\"85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4\",\"96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b\",\"9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493\",\"b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6\",\"bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde\",\"20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe\"]},\"ecdsa-v1\":{}}}"},"signature":"743ed9900aba5a1ba3ba9d862628cdc5cca27974c40c4ab64618709021b3fbb13216a3efc733be260025da487ae9b63a8290d555bdc8da6324deff149fc7b110"},"attestation":{"outcome":"48935","schemes":{"olivia-v1":{"scalars":["1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484","72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6","051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e","847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7","5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998","368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee","9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999","fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738","44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086","587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d","010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955","5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520","06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd","fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe","0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068","c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f","86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269","fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db","41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d","ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"]},"ecdsa-v1":{"signature":"1d9a5e2336883cc6b440ff40e16ee44f8af2ba9313e46f1e4cd417f7dba7686279b0216e4b0b5fcf0c650dbad98fdefcf5ef16b49d63651a87f80caddd472384"}},"time":"2021-10-04T22:00:15"}}"#;

            let deserialized = serde_json::from_str::<oracle::Attestation>(json).unwrap();
            let expected = oracle::Attestation {
                id: BitMexPriceEventId::with_20_digits(datetime!(2021-10-04 22:00:00).assume_utc()),
                price: 48935,
                scalars: vec![
                    "1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484"
                        .parse()
                        .unwrap(),
                    "72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6"
                        .parse()
                        .unwrap(),
                    "051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e"
                        .parse()
                        .unwrap(),
                    "847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7"
                        .parse()
                        .unwrap(),
                    "5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998"
                        .parse()
                        .unwrap(),
                    "368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee"
                        .parse()
                        .unwrap(),
                    "9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999"
                        .parse()
                        .unwrap(),
                    "fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738"
                        .parse()
                        .unwrap(),
                    "44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086"
                        .parse()
                        .unwrap(),
                    "587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d"
                        .parse()
                        .unwrap(),
                    "010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955"
                        .parse()
                        .unwrap(),
                    "5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520"
                        .parse()
                        .unwrap(),
                    "06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd"
                        .parse()
                        .unwrap(),
                    "fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe"
                        .parse()
                        .unwrap(),
                    "0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068"
                        .parse()
                        .unwrap(),
                    "c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f"
                        .parse()
                        .unwrap(),
                    "86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269"
                        .parse()
                        .unwrap(),
                    "fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db"
                        .parse()
                        .unwrap(),
                    "41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d"
                        .parse()
                        .unwrap(),
                    "ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"
                        .parse()
                        .unwrap(),
                ],
            };

            assert_eq!(deserialized, expected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn next_event_id_after_timestamp() {
        let event_id =
            next_announcement_after(datetime!(2021-09-23 10:40:00).assume_utc()).unwrap();

        assert_eq!(
            event_id.to_string(),
            "/x/BitMEX/BXBT/2021-09-23T11:00:00.price?n=20"
        );
    }

    #[test]
    fn next_event_id_is_midnight_next_day() {
        let event_id =
            next_announcement_after(datetime!(2021-09-23 23:40:00).assume_utc()).unwrap();

        assert_eq!(
            event_id.to_string(),
            "/x/BitMEX/BXBT/2021-09-24T00:00:00.price?n=20"
        );
    }
}
