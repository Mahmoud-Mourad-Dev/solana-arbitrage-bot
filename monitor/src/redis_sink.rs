//! Redis publisher — emits the SAME payload contract as the TS RedisSink:
//! LPUSH + LTRIM + PUBLISH of the opportunity JSON. Pool-state mirroring
//! (the TS HSET pipeline) is observability-only and intentionally deferred;
//! the executor consumes only the channel, which this reproduces exactly.

use anyhow::{Context, Result};
use arb_common::opportunity::Opportunity;
use redis::aio::ConnectionManager;

pub struct RedisSink {
    conn: ConnectionManager,
    channel: String,
    list: String,
    list_max: isize,
}

impl RedisSink {
    pub async fn connect(
        url: &str,
        channel: String,
        list: String,
        list_max: isize,
    ) -> Result<Self> {
        let client = redis::Client::open(url).context("redis url")?;
        let conn = ConnectionManager::new(client)
            .await
            .context("redis connect")?;
        Ok(Self {
            conn,
            channel,
            list,
            list_max,
        })
    }

    /// LPUSH + LTRIM + PUBLISH in one pipeline, matching TS ordering.
    pub async fn publish_opportunity(&mut self, opp: &Opportunity) -> Result<()> {
        let payload = serde_json::to_string(opp).context("serialize opportunity")?;
        let mut pipe = redis::pipe();
        pipe.atomic()
            .lpush(&self.list, &payload)
            .ltrim(&self.list, 0, self.list_max - 1)
            .publish(&self.channel, &payload);
        let _: () = pipe
            .query_async(&mut self.conn)
            .await
            .context("redis publish pipeline")?;
        Ok(())
    }
}
