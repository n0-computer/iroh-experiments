use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use iroh::SecretKey;
use iroh_blobs::HashAndFormat;
use pkarr::{
    dns::{
        rdata::{RData, TXT},
        Name, ResourceRecord, CLASS,
    },
    SignedPacket, Timestamp,
};
use tokio_util::task::AbortOnDropHandle;

/// The key for the content of an IPNS record.
const CONTENT_KEY: &str = "_content.iroh.";
/// Republish delay for the DHT. This is only for when the info does not change.
/// If the info changes, it will be published immediately.
const REPUBLISH_DELAY: Duration = Duration::from_secs(60 * 60);
/// Initial publish delay. This is to avoid spamming the DHT when there are
/// frequent network changes at startup.
const INITIAL_PUBLISH_DELAY: Duration = Duration::from_millis(500);

/// An IPNS record.
///
/// This is a record that can be published to the iroh pkarr naming system.
#[derive(Clone, Debug)]
pub enum Record {
    /// Content only.
    Content { content: HashAndFormat },
}

impl Record {
    fn content(&self) -> Option<&HashAndFormat> {
        match self {
            Record::Content { content } => Some(content),
        }
    }
}

/// An iroh pkarr naming system publisher constantly republishes any number of records.
#[derive(Clone, Debug)]
pub struct IPNS(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    pkarr: Arc<pkarr::Client>,
    packets: Mutex<BTreeMap<iroh::PublicKey, (Record, AbortOnDropHandle<()>)>>,
}

impl IPNS {
    /// Create a new default IPNS publisher.
    pub fn new() -> anyhow::Result<Self> {
        let inner = Inner {
            pkarr: Arc::new(pkarr::ClientBuilder::default().build()?),
            packets: Mutex::new(BTreeMap::default()),
        };
        Ok(Self(Arc::new(inner)))
    }

    /// Publish a record for a keypair, or stop publishing if `record` is `None`.
    pub async fn publish(
        &self,
        secret_key: SecretKey,
        record: Option<Record>,
    ) -> anyhow::Result<()> {
        let key = secret_key.public();
        if let Some(record) = record {
            let pkarr = self.0.pkarr.clone();
            let signed_packet: SignedPacket = Self::to_signed_packet(&secret_key, &record, 0)?;
            let publish_task = tokio::spawn(async move {
                tokio::time::sleep(INITIAL_PUBLISH_DELAY).await;
                loop {
                    let res = pkarr.publish(&signed_packet, None).await;
                    match res {
                        Ok(()) => {
                            tracing::info!("Published record");
                        }
                        Err(e) => {
                            tracing::warn!("Failed to publish record: {}", e);
                        }
                    }
                    tokio::time::sleep(REPUBLISH_DELAY).await;
                }
            });
            let mut packets = self.0.packets.lock().unwrap();
            packets.insert(key, (record, AbortOnDropHandle::new(publish_task)));
        } else {
            let mut packets = self.0.packets.lock().unwrap();
            packets.remove(&key);
        };
        Ok(())
    }

    /// Resolve a record for a public key.
    pub async fn resolve(&self, public_key: iroh::PublicKey) -> anyhow::Result<Option<Record>> {
        let public_key =
            pkarr::PublicKey::try_from(public_key.as_bytes()).context("invalid public key")?;
        let packet = self.0.pkarr.resolve(&public_key).await;
        packet.map(Self::to_record).transpose()
    }

    /// Produce a signed packet for a record.
    fn to_signed_packet(
        secret_key: &SecretKey,
        record: &Record,
        ttl: u32,
    ) -> anyhow::Result<SignedPacket> {
        let keypair = pkarr::Keypair::from_secret_key(&secret_key.to_bytes());
        let mut answers = Vec::new();
        if let Some(content) = record.content() {
            answers.push(ResourceRecord::new(
                Name::new(CONTENT_KEY).unwrap(),
                CLASS::IN,
                ttl,
                RData::TXT(TXT::try_from(content.to_string().as_str())?.into_owned()),
            ));
        }
        Ok(SignedPacket::new(&keypair, &answers, Timestamp::now())?)
    }

    fn to_record(packet: SignedPacket) -> anyhow::Result<Record> {
        // first DERP URL, if any
        let content = packet
            .resource_records(CONTENT_KEY)
            .filter_map(filter_txt)
            .map(|url| anyhow::Ok(HashAndFormat::from_str(&url)?))
            .next()
            .transpose()?
            .context("no content found in IPNS record")?;

        Ok(Record::Content { content })
    }
}

fn filter_txt(rr: &ResourceRecord) -> Option<String> {
    if rr.class != CLASS::IN {
        return None;
    }
    if let RData::TXT(txt) = &rr.rdata {
        String::try_from(txt.clone()).ok()
    } else {
        None
    }
}
