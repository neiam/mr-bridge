use clap::Parser;
use rumqttc::QoS;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttBrokerConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_client_id")]
    pub client_id: String,
    /// Maximum MQTT packet size in bytes for incoming and outgoing packets.
    /// rumqttc defaults to 10 KiB, which is too small for large retained
    /// payloads such as Zigbee2MQTT's `bridge/definitions`; default 5 MiB.
    #[serde(default = "default_max_packet_size")]
    pub max_packet_size: usize,
    /// Keep-alive interval in seconds. rumqttc declares the link dead ("last
    /// pingreq isn't acked") if a PINGRESP doesn't return within it, so raise it
    /// for a high-latency or flaky far link to avoid needless reconnects; lower
    /// it to detect a dead peer faster. Default 30.
    #[serde(default = "default_keepalive_secs")]
    pub keepalive_secs: u64,
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_keepalive_secs() -> u64 {
    30
}

fn default_max_packet_size() -> usize {
    5 * 1024 * 1024
}

fn default_client_id() -> String {
    format!("mr-bridge-{}", uuid::Uuid::new_v4())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    NearToFar,
    FarToNear,
    Wherever,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRule {
    /// Supports MQTT wildcards (+ for single level, # for multi-level)
    pub topic: String,
    /// Which direction we're forwarding messages
    pub direction: Direction,
    /// Log every message that matches the topic we're bridging
    #[serde(default)]
    pub logging: bool,
    /// Quality of Service level (0, 1, or 2)
    #[serde(default = "default_qos")]
    pub qos: u8,
    /// Rewrite the topic on the way through: a message whose topic starts with
    /// `from` is published on the other broker with that prefix swapped for
    /// `to`. One-way rules only — on a `wherever` rule the reverse trip would
    /// need the opposite rewrite, which is a second rule's job.
    #[serde(default)]
    pub remap: Option<Remap>,
    /// Override the retain flag on forwarded messages. Unset keeps whatever the
    /// source said. `true` is for a source that cannot retain — RabbitMQ keeps
    /// nothing published over AMQP — when the destination should hold the last
    /// value for subscribers that arrive later.
    #[serde(default)]
    pub retain: Option<bool>,
}

/// A topic prefix rewrite. Plain string prefixes rather than wildcards, so
/// `from = "a/b/"` turns `a/b/c/d` into `<to>c/d`. A topic without the prefix
/// is forwarded unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Remap {
    pub from: String,
    pub to: String,
}

fn default_qos() -> u8 {
    0
}

impl BridgeRule {
    pub fn qos(&self) -> QoS {
        match self.qos {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            2 => QoS::ExactlyOnce,
            _ => QoS::AtMostOnce,
        }
    }

    /// The topic a message arriving on `topic` is published to on the other side.
    pub fn forward_topic(&self, topic: &str) -> String {
        match &self.remap {
            Some(Remap { from, to }) => match topic.strip_prefix(from.as_str()) {
                Some(rest) => format!("{to}{rest}"),
                None => topic.to_string(),
            },
            None => topic.to_string(),
        }
    }

    /// The retain flag to forward with, given the one the message arrived with.
    pub fn forward_retain(&self, incoming: bool) -> bool {
        self.retain.unwrap_or(incoming)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Loop-prevention window in seconds. When > 0, a forwarded message is
    /// fingerprinted (topic + payload) and its echo — the copy the destination
    /// broker delivers back to our own subscription — is dropped instead of
    /// being forwarded again, which is what lets you bridge overlapping topics
    /// bidirectionally (e.g. `direction = "wherever"` on `#`) without a storm.
    /// 0 (default) disables it; use it only for symmetric/overlapping bridges.
    #[serde(default = "default_dedup_window_secs")]
    pub dedup_window_secs: u64,
    pub near: MqttBrokerConfig,
    pub far: MqttBrokerConfig,
    pub rules: Vec<BridgeRule>,
}

fn default_dedup_window_secs() -> u64 {
    0
}

impl BridgeConfig {
    /// Load configuration from a file (supports TOML and JSON)
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let ext = path.as_ref().extension().and_then(|s| s.to_str());

        let config: Self = match ext {
            Some("toml") => toml::from_str(&content)?,
            Some("json") => serde_json::from_str(&content)?,
            _ => {
                // Try TOML first, then JSON
                toml::from_str(&content).or_else(|_| serde_json::from_str(&content))?
            }
        };

        config.validate()?;
        Ok(config)
    }

    /// Reject what parses but cannot work.
    pub fn validate(&self) -> anyhow::Result<()> {
        for rule in &self.rules {
            if rule.remap.is_some() && matches!(rule.direction, Direction::Wherever) {
                anyhow::bail!(
                    "rule '{}': remap needs a one-way direction; use a near_to_far and a far_to_near rule instead of wherever",
                    rule.topic
                );
            }
        }
        Ok(())
    }
}

#[derive(Parser, Debug)]
#[command(name = "mr-bridge")]
#[command(about = "MQTT Bridge - Bridge topics between two MQTT brokers", long_about = None)]
pub struct Args {
    /// Path to configuration file (TOML or JSON)
    #[arg(short, long, env = "MR_BRIDGE_CONFIG")]
    pub config: std::path::PathBuf,

    /// Optional topic to listen for reload commands
    /// When a message is received on this topic, the config file will be reloaded
    #[arg(short, long, env = "MR_BRIDGE_RELOAD_TOPIC")]
    pub reload_topic: Option<String>,

    /// Which broker to subscribe to reload topic on (near or far)
    #[arg(long, env = "MR_BRIDGE_RELOAD_BROKER", default_value = "near")]
    pub reload_broker: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(toml_rule: &str) -> BridgeRule {
        toml::from_str(toml_rule).unwrap()
    }

    #[test]
    fn remap_swaps_the_prefix() {
        let r = rule(
            r#"
            topic = "board/ha/#"
            direction = "far_to_near"
            remap = { from = "board/ha/", to = "homeassistant/" }
            "#,
        );
        assert_eq!(
            r.forward_topic("board/ha/device/x/config"),
            "homeassistant/device/x/config"
        );
        // Outside the prefix: unchanged.
        assert_eq!(r.forward_topic("board/plus"), "board/plus");
    }

    #[test]
    fn remap_can_rename_a_whole_topic() {
        let r = rule(
            r#"
            topic = "homeassistant/status"
            direction = "near_to_far"
            remap = { from = "homeassistant/status", to = "board/publish/ha" }
            "#,
        );
        assert_eq!(r.forward_topic("homeassistant/status"), "board/publish/ha");
    }

    #[test]
    fn no_remap_and_no_retain_override_pass_through() {
        let r = rule(
            r#"
            topic = "a/#"
            direction = "wherever"
            "#,
        );
        assert_eq!(r.forward_topic("a/b"), "a/b");
        assert!(r.forward_retain(true));
        assert!(!r.forward_retain(false));
    }

    #[test]
    fn retain_override_wins() {
        let r = rule(
            r#"
            topic = "a/#"
            direction = "far_to_near"
            retain = true
            "#,
        );
        assert!(r.forward_retain(false));
    }

    #[test]
    fn remap_on_wherever_is_refused() {
        let config: BridgeConfig = toml::from_str(
            r#"
            [near]
            host = "localhost"
            [far]
            host = "localhost"
            [[rules]]
            topic = "a/#"
            direction = "wherever"
            remap = { from = "a/", to = "b/" }
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
