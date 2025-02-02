use crate::harness::maia::OliviaData;
use daemon::model::BitMexPriceEventId;
use daemon::oracle;
use mockall::*;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use xtra_productivity::xtra_productivity;

/// Test Stub simulating the Oracle actor.
/// Serves as an entrypoint for injected mock handlers.
pub struct OracleActor {
    pub mock: Arc<Mutex<dyn Oracle + Send>>,
}

impl xtra::Actor for OracleActor {}
impl Oracle for OracleActor {}

#[xtra_productivity(message_impl = false)]
impl OracleActor {
    async fn handle(&mut self, msg: oracle::GetAnnouncement) -> Option<oracle::Announcement> {
        self.mock.lock().await.get_announcement(msg)
    }

    async fn handle(&mut self, msg: oracle::MonitorAttestation) {
        self.mock.lock().await.monitor_attestation(msg)
    }

    async fn handle(&mut self, msg: oracle::Sync) {
        self.mock.lock().await.sync(msg)
    }
}

#[automock]
pub trait Oracle {
    fn get_announcement(&mut self, _msg: oracle::GetAnnouncement) -> Option<oracle::Announcement> {
        unreachable!("mockall will reimplement this method")
    }

    fn monitor_attestation(&mut self, _msg: oracle::MonitorAttestation) {
        unreachable!("mockall will reimplement this method")
    }

    fn sync(&mut self, _msg: oracle::Sync) {
        unreachable!("mockall will reimplement this method")
    }
}

pub fn dummy_announcement() -> oracle::Announcement {
    let announcement = OliviaData::example_0().announcement();

    oracle::Announcement {
        id: BitMexPriceEventId::new(OffsetDateTime::UNIX_EPOCH, 0),
        expected_outcome_time: OffsetDateTime::now_utc(),
        nonce_pks: announcement.nonce_pks,
    }
}
