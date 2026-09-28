//! Configuration from the environment. Every key is listed in `docs/configuration.md`.
//!
//! Only the control socket and the RTP range are required. Redis (discovery, status, the compose queue) and S3
//! (upload) are each optional: without Redis the recorder is found by its address and reports nothing; without S3
//! the recording stays in the spool and the composite is written there.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// This recorder's id: the field in the `recorders:` registry hash and the `recorderId` in every manifest.
    pub id: String,
    /// Address controllers connect to (announced), and the port of the control socket.
    pub host: String,
    pub port: u16,
    /// Listen address for the control socket.
    pub listen_host: String,
    /// Address the SFU sends RTP to; we listen on `rtc_port_min..=rtc_port_max` there.
    pub rtc_ip: String,
    pub announced_ip: String,
    pub rtc_port_min: u16,
    pub rtc_port_max: u16,
    /// Handshake secret controllers must present (`RECORDER_SECRET`).
    pub secret: String,
    /// Discovery, status and the compose wake-up list. `None` runs without any of them.
    pub redis: Option<RedisConfig>,
    pub spool_dir: PathBuf,
    /// Refuse a new capture below this much free spool space.
    pub min_free_bytes: u64,
    /// Hard cap on one recording's length (two hours by default).
    pub max_recording_ms: u64,
    /// After losing its controller without a reconnect, a capture is finalised on its own.
    pub orphan_grace_ms: u64,
    pub jitter_buffer_ms: u32,
    /// Where finished recordings go. `None` keeps them in the spool.
    pub s3: Option<S3Config>,
    /// Which roles this process runs: capture, compose, or both (default).
    pub role: Role,
    pub policy_dir: PathBuf,
    pub compose: ComposeConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Capture,
    Compose,
    Both,
}

impl Role {
    pub fn captures(self) -> bool {
        self != Role::Compose
    }
    pub fn composes(self) -> bool {
        self != Role::Capture
    }
}

/// Composite output: per deployment, not per recording.
#[derive(Clone, Debug)]
pub struct ComposeConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub nice: i32,
    /// Compose while a capture is live on this recorder. Off by default: on a small box the encoder's threads
    /// stall the capture threads and packets are lost (a 2-core cap composes after the meetings).
    pub while_capturing: bool,
    pub font: PathBuf,
    /// Material Icons; when missing the status icons are drawn as shapes.
    pub icon_font: PathBuf,
    /// Product name on the title card; empty draws none.
    pub brand: String,
}

/// The recordings bucket: any S3-compatible store (AWS S3, MinIO, Garage, Ceph RGW).
#[derive(Clone, Debug)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub force_path_style: bool,
}

#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    pub db: i64,
    pub tls: bool,
}

impl RedisConfig {
    /// Channel names are scoped by database number, `<db>:<channel>`, so several deployments can share one
    /// Redis without hearing each other.
    pub fn channel(&self, name: &str) -> String {
        format!("{}:{}", self.db, name)
    }
}

pub const DEFAULT_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
pub const DEFAULT_ICON_FONT: &str =
    "/usr/share/fonts/truetype/material-design-icons-iconfont/MaterialIcons-Regular.ttf";
pub const DEFAULT_POLICY_DIR: &str = "/etc/recorder/policies";

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set"))
}

fn var_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse<T: std::str::FromStr>(name: &str, value: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|e| anyhow!("{name}={value:?} is not valid: {e}"))
}

fn is_true(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => v == "true" || v == "1",
        Err(_) => default,
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let rtc_port_min: u16 = parse(
            "RECORDER_RTC_MIN_PORT",
            &var_or("RECORDER_RTC_MIN_PORT", "41000"),
        )?;
        let rtc_port_max: u16 = parse(
            "RECORDER_RTC_MAX_PORT",
            &var_or("RECORDER_RTC_MAX_PORT", "41999"),
        )?;
        if rtc_port_max <= rtc_port_min + 1 {
            return Err(anyhow!(
                "RECORDER_RTC_MAX_PORT must leave room for at least one RTP/RTCP pair"
            ));
        }
        let announced_ip = var("RECORDER_ANNOUNCED_IP")?;
        // `RECORDING_SECRET` is the name the first deployments used; both are read.
        let secret = std::env::var("RECORDER_SECRET")
            .or_else(|_| std::env::var("RECORDING_SECRET"))
            .context("RECORDER_SECRET is not set")?;
        let id = std::env::var("RECORDER_ID")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "recorder".to_string());

        let redis = match std::env::var("REDIS_HOST") {
            Ok(host) if !host.is_empty() => Some(RedisConfig {
                host,
                port: parse("REDIS_PORT", &var_or("REDIS_PORT", "6379"))?,
                password: std::env::var("REDIS_PASSWORD")
                    .ok()
                    .filter(|p| !p.is_empty()),
                db: parse("REDIS_DB", &var_or("REDIS_DB", "0"))?,
                tls: is_true("REDIS_TLS", false),
            }),
            _ => None,
        };

        let s3 = match std::env::var("S3_ENDPOINT") {
            Ok(endpoint) if !endpoint.is_empty() => Some(S3Config {
                endpoint,
                region: var_or("S3_REGION", "us-east-1"),
                bucket: var("S3_BUCKET")?,
                access_key_id: var("S3_ACCESS_KEY_ID")?,
                secret_access_key: var("S3_SECRET_ACCESS_KEY")?,
                force_path_style: is_true("S3_FORCE_PATH_STYLE", true),
            }),
            _ => None,
        };

        Ok(Self {
            id,
            host: var_or("RECORDER_HOST", &announced_ip),
            port: parse("RECORDER_PORT", &var_or("RECORDER_PORT", "3100"))?,
            listen_host: var_or("RECORDER_LISTEN_HOST", "0.0.0.0"),
            rtc_ip: var_or("RECORDER_RTC_IP", "0.0.0.0"),
            announced_ip,
            rtc_port_min,
            rtc_port_max,
            secret,
            redis,
            spool_dir: PathBuf::from(var_or("RECORDER_SPOOL_DIR", "/spool")),
            min_free_bytes: parse(
                "RECORDER_MIN_FREE_BYTES",
                &var_or("RECORDER_MIN_FREE_BYTES", "5368709120"),
            )?,
            max_recording_ms: parse(
                "RECORDER_MAX_RECORDING_MS",
                &var_or("RECORDER_MAX_RECORDING_MS", "7200000"),
            )?,
            orphan_grace_ms: parse(
                "RECORDER_ORPHAN_GRACE_MS",
                &var_or("RECORDER_ORPHAN_GRACE_MS", "30000"),
            )?,
            jitter_buffer_ms: parse(
                "RECORDER_JITTER_BUFFER_MS",
                &var_or("RECORDER_JITTER_BUFFER_MS", "3000"),
            )?,
            role: match var_or("RECORDER_ROLE", "both").as_str() {
                "capture" => Role::Capture,
                "compose" => Role::Compose,
                "both" => Role::Both,
                other => return Err(anyhow!("RECORDER_ROLE={other:?}: capture, compose or both")),
            },
            policy_dir: PathBuf::from(var_or("RECORDER_POLICY_DIR", DEFAULT_POLICY_DIR)),
            compose: ComposeConfig {
                width: parse(
                    "RECORDER_COMPOSE_WIDTH",
                    &var_or("RECORDER_COMPOSE_WIDTH", "1280"),
                )?,
                height: parse(
                    "RECORDER_COMPOSE_HEIGHT",
                    &var_or("RECORDER_COMPOSE_HEIGHT", "720"),
                )?,
                fps: parse(
                    "RECORDER_COMPOSE_FPS",
                    &var_or("RECORDER_COMPOSE_FPS", "30"),
                )?,
                video_kbps: parse(
                    "RECORDER_COMPOSE_VIDEO_KBPS",
                    &var_or("RECORDER_COMPOSE_VIDEO_KBPS", "2500"),
                )?,
                audio_kbps: parse(
                    "RECORDER_COMPOSE_AUDIO_KBPS",
                    &var_or("RECORDER_COMPOSE_AUDIO_KBPS", "128"),
                )?,
                nice: parse(
                    "RECORDER_COMPOSE_NICE",
                    &var_or("RECORDER_COMPOSE_NICE", "15"),
                )?,
                while_capturing: is_true("RECORDER_COMPOSE_WHILE_CAPTURING", false),
                icon_font: PathBuf::from(var_or("RECORDER_COMPOSE_ICON_FONT", DEFAULT_ICON_FONT)),
                brand: var_or("RECORDER_COMPOSE_BRAND", ""),
                font: PathBuf::from(var_or("RECORDER_FONT", DEFAULT_FONT)),
            },
            s3,
        })
    }
}
