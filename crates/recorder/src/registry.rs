//! Redis: the `recorders:` registry controllers read (`docs/protocol.md`), load announcements, and the
//! `recording-events` status channel the application subscribes to. All optional: see `config.rs`.

use crate::config::Config;
use crate::wire::{RegistryEntry, StatusEvent};
use anyhow::{Context, Result};
use redis::AsyncCommands;

const REGISTRY_HASH: &str = "recorders:";
const REGISTRY_CHANNEL: &str = "recorders";
const STATUS_CHANNEL: &str = "recording-events";
/// Wake-up list for the compose worker: a hint, the manifests in the bucket are the truth.
const COMPOSE_LIST: &str = "recording-compose";
const COMPOSE_CLAIM: &str = "recording-compose-claim:";

#[derive(Clone)]
pub struct Registry {
    conn: redis::aio::MultiplexedConnection,
    id: String,
    host: String,
    port: u16,
    registry_channel: String,
    status_channel: String,
}

impl Registry {
    /// The general connection: registry, load, status. The crate's default response timeout is 500 ms, too
    /// tight for a busy Redis on the same box as the media stack.
    pub async fn connect(config: &Config) -> Result<Self> {
        Self::connect_with(config, std::time::Duration::from_secs(5)).await
    }

    /// The compose worker's own connection: its BLPOP blocks up to `wait_compose`'s timeout.
    pub async fn connect_blocking(config: &Config) -> Result<Self> {
        Self::connect_with(config, std::time::Duration::from_secs(60)).await
    }

    async fn connect_with(config: &Config, response_timeout: std::time::Duration) -> Result<Self> {
        let redis = config
            .redis
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("redis is not configured (REDIS_HOST)"))?;
        let scheme = if redis.tls { "rediss" } else { "redis" };
        let auth = match &redis.password {
            Some(p) => format!(":{}@", urlencode(p)),
            None => String::new(),
        };
        let url = format!(
            "{scheme}://{auth}{}:{}/{}",
            redis.host, redis.port, redis.db
        );
        let client = redis::Client::open(url).context("redis url")?;
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .context("redis connect")?;
        conn.set_response_timeout(response_timeout);
        Ok(Self {
            conn,
            id: config.id.clone(),
            host: config.host.clone(),
            port: config.port,
            registry_channel: redis.channel(REGISTRY_CHANNEL),
            status_channel: redis.channel(STATUS_CHANNEL),
        })
    }

    fn entry(&self, load: f64) -> String {
        serde_json::to_string(&RegistryEntry {
            id: &self.id,
            host: &self.host,
            port: self.port,
            tls: false,
            load,
        })
        .expect("registry entry serialises")
    }

    /// Announce this recorder: HSET into the hash and RECORDER_ADDED on the channel.
    pub async fn register(&mut self, load: f64) -> Result<()> {
        let entry = self.entry(load);
        let _: () = self.conn.hset(REGISTRY_HASH, &self.id, &entry).await?;
        let msg = serde_json::json!({ "type": "RECORDER_ADDED", "message": serde_json::from_str::<serde_json::Value>(&entry)? });
        let _: () = self
            .conn
            .publish(&self.registry_channel, msg.to_string())
            .await?;
        Ok(())
    }

    pub async fn publish_load(&mut self, load: f64) -> Result<()> {
        let entry = self.entry(load);
        let _: () = self.conn.hset(REGISTRY_HASH, &self.id, &entry).await?;
        let msg = serde_json::json!({ "type": "RECORDER_LOAD", "message": { "recorderId": self.id, "load": load } });
        let _: () = self
            .conn
            .publish(&self.registry_channel, msg.to_string())
            .await?;
        Ok(())
    }

    pub async fn unregister(&mut self) -> Result<()> {
        let _: () = self.conn.hdel(REGISTRY_HASH, &self.id).await?;
        let msg = serde_json::json!({ "type": "RECORDER_REMOVED", "message": self.id });
        let _: () = self
            .conn
            .publish(&self.registry_channel, msg.to_string())
            .await?;
        Ok(())
    }

    /// Status for whoever tracks recordings. A hint: the manifest in the bucket is the truth for `ready`.
    /// The wake-up carries the bucket prefix, so a worker without the spool knows what to fetch.
    pub async fn enqueue_compose(&mut self, prefix: &str) -> Result<()> {
        let _: () = self.conn.rpush(COMPOSE_LIST, prefix).await?;
        Ok(())
    }

    /// Claim a recording for compose: true when this worker got it, false when another one holds it. The claim
    /// expires on its own (a worker that dies mid-job must not block the recording forever) and is released when
    /// the job ends either way; a finished recording is recognised by its manifest in the bucket, not by the claim.
    pub async fn claim_compose(&mut self, recording_id: &str, ttl_s: u64) -> Result<bool> {
        let set: Option<String> = redis::cmd("SET")
            .arg(format!("{COMPOSE_CLAIM}{recording_id}"))
            .arg(&self.id)
            .arg("NX")
            .arg("EX")
            .arg(ttl_s)
            .query_async(&mut self.conn)
            .await?;
        Ok(set.is_some())
    }

    pub async fn release_compose(&mut self, recording_id: &str) -> Result<()> {
        let _: () = self
            .conn
            .del(format!("{COMPOSE_CLAIM}{recording_id}"))
            .await?;
        Ok(())
    }

    /// Blocks up to `timeout_s` for the next compose wake-up.
    pub async fn wait_compose(&mut self, timeout_s: u64) -> Result<Option<String>> {
        let item: Option<(String, String)> =
            self.conn.blpop(COMPOSE_LIST, timeout_s as f64).await?;
        Ok(item.map(|(_, v)| v))
    }

    pub async fn publish_status(&mut self, event: &StatusEvent) -> Result<()> {
        let _: () = self
            .conn
            .publish(&self.status_channel, serde_json::to_string(event)?)
            .await?;
        Ok(())
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
