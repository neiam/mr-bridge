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
}

fn default_mqtt_port() -> u16 {
    1883
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub near: MqttBrokerConfig,
    pub far: MqttBrokerConfig,
    pub rules: Vec<BridgeRule>,
}

impl BridgeConfig {
    /// Load configuration from a file (supports TOML and JSON)
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let ext = path.as_ref().extension().and_then(|s| s.to_str());

        let config = match ext {
            Some("toml") => toml::from_str(&content)?,
            Some("json") => serde_json::from_str(&content)?,
            _ => {
                // Try TOML first, then JSON
                toml::from_str(&content).or_else(|_| serde_json::from_str(&content))?
            }
        };

        Ok(config)
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
