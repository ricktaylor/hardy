use hardy_async::async_trait;
use hardy_async::sync::spin::Once;
use hardy_bpa::Bytes;
use hardy_bpa::services::{Service, ServiceContext, StatusNotify};
use hardy_bpv7::bpsec;
use hardy_bpv7::bundle::{Id as BundleId, ParsedBundle};
use hardy_bpv7::editor::{Chunk, Editor};
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use tracing::{debug, warn};

pub struct EchoService {
    ctx: Once<ServiceContext>,
}

impl Default for EchoService {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoService {
    pub fn new() -> Self {
        EchoService { ctx: Once::new() }
    }

    async fn echo(&self, data: Bytes) -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
        if let Some(ctx) = self.ctx.get() {
            let bundle = ParsedBundle::parse(&data, bpsec::no_keys)
                .inspect_err(|e| debug!("Failed to parse incoming bundle: {e:?}"))?
                .bundle;

            debug!(
                source = %bundle.id.source,
                destination = %bundle.destination,
                "Received bundle, reflecting back to source"
            );

            let chunks = Editor::new(&bundle, &data)
                .with_source(bundle.destination.clone())
                .map_err(|(_, e)| {
                    debug!("Failed to set source Eid: {e:?}");
                    e
                })?
                .with_destination(bundle.id.source.clone())
                .map_err(|(_, e)| {
                    debug!("Failed to set destination Eid: {e:?}");
                    e
                })?
                .rebuild()
                .inspect_err(|e| debug!("Failed to update bundle: {e:?}"))?;

            debug!(
                source = %bundle.destination,
                destination = %bundle.id.source,
                "Sending echo reply"
            );

            let reply = match data.try_into_mut() {
                Ok(buf) => {
                    let mut vec = buf.into();
                    Chunk::flatten_inplace(chunks, &mut vec);
                    Bytes::from(vec)
                }
                Err(original) => Bytes::from(Chunk::flatten(chunks, &original)),
            };

            ctx.send_raw(reply).await.inspect_err(|e| {
                warn!("Failed to send reply: {e:?}");
            })?;
        }
        Ok(())
    }
}

#[async_trait]
impl Service for EchoService {
    async fn on_register(&self, _source: &Eid, ctx: ServiceContext) {
        self.ctx.call_once(|| ctx);
    }

    async fn on_unregister(&self) {}

    async fn on_status_notify(
        &self,
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
    }

    async fn on_receive(&self, data: Bytes, _expiry: time::OffsetDateTime) {
        if let Err(e) = self.echo(data).await {
            warn!("Echo failed: {e:?}");
        }
    }
}
