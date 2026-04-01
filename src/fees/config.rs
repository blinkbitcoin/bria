use serde::{Deserialize, Serialize};

use super::blockstream::BlockstreamConfig;
use super::mempool_space::MempoolSpaceConfig;

#[serde_with::serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeesConfig {
    #[serde(default)]
    pub mempool_space: MempoolSpaceConfig,
    #[serde(default)]
    pub blockstream: BlockstreamConfig,
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: std::time::Duration,
    #[serde_as(as = "serde_with::DurationSeconds<u64>")]
    #[serde(default = "default_stale_ttl")]
    pub stale_ttl: std::time::Duration,
    #[serde(default = "default_enable_stale_on_error")]
    pub enable_stale_on_error: bool,
}

impl Default for FeesConfig {
    fn default() -> Self {
        Self {
            mempool_space: MempoolSpaceConfig::default(),
            blockstream: BlockstreamConfig::default(),
            cache_ttl: default_cache_ttl(),
            stale_ttl: default_stale_ttl(),
            enable_stale_on_error: default_enable_stale_on_error(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FeeCacheConfig {
    pub cache_ttl: std::time::Duration,
    pub stale_ttl: std::time::Duration,
    pub enable_stale_on_error: bool,
}

impl From<&FeesConfig> for FeeCacheConfig {
    fn from(config: &FeesConfig) -> Self {
        Self {
            cache_ttl: config.cache_ttl,
            stale_ttl: config.stale_ttl,
            enable_stale_on_error: config.enable_stale_on_error,
        }
    }
}

fn default_cache_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(15)
}

fn default_stale_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(120)
}

fn default_enable_stale_on_error() -> bool {
    true
}
