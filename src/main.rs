use anyhow::{Context, Result};
use clap::Parser;
use mr_bridge::{Args, BridgeConfig, Direction, MqttBrokerConfig};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, Publish, QoS};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

struct Bridge {
    near_client: AsyncClient,
    far_client: AsyncClient,
    config: Arc<RwLock<BridgeConfig>>,
    config_path: std::path::PathBuf,
    reload_topic: Option<String>,
    reload_broker: String,
}

impl Bridge {
    /// Build the bridge plus the two broker event loops (kept separate so each
    /// can be polled in its own task).
    async fn new(args: &Args) -> Result<(Self, EventLoop, EventLoop)> {
        let config =
            BridgeConfig::load_from_file(&args.config).context("Failed to load configuration")?;

        let (near_client, near_eventloop) = create_mqtt_client(&config.near, "near")?;
        let (far_client, far_eventloop) = create_mqtt_client(&config.far, "far")?;

        let bridge = Self {
            near_client,
            far_client,
            config: Arc::new(RwLock::new(config)),
            config_path: args.config.clone(),
            reload_topic: args.reload_topic.clone(),
            reload_broker: args.reload_broker.clone(),
        };
        Ok((bridge, near_eventloop, far_eventloop))
    }

    async fn subscribe_to_topics(&self) -> Result<()> {
        let config = self.config.read().await;

        for rule in &config.rules {
            match rule.direction {
                Direction::NearToFar => {
                    info!(
                        "Subscribing to '{}' on NEAR broker (forwarding to FAR)",
                        rule.topic
                    );
                    self.near_client
                        .subscribe(&rule.topic, rule.qos())
                        .await
                        .context(format!(
                            "Failed to subscribe to '{}' on near broker",
                            rule.topic
                        ))?;
                }
                Direction::FarToNear => {
                    info!(
                        "Subscribing to '{}' on FAR broker (forwarding to NEAR)",
                        rule.topic
                    );
                    self.far_client
                        .subscribe(&rule.topic, rule.qos())
                        .await
                        .context(format!(
                            "Failed to subscribe to '{}' on far broker",
                            rule.topic
                        ))?;
                }
                Direction::Wherever => {
                    info!(
                        "Subscribing to '{}' on BOTH brokers (bidirectional)",
                        rule.topic
                    );
                    self.near_client
                        .subscribe(&rule.topic, rule.qos())
                        .await
                        .context(format!(
                            "Failed to subscribe to '{}' on near broker",
                            rule.topic
                        ))?;
                    self.far_client
                        .subscribe(&rule.topic, rule.qos())
                        .await
                        .context(format!(
                            "Failed to subscribe to '{}' on far broker",
                            rule.topic
                        ))?;
                }
            }
        }

        // Subscribe to reload topic if configured
        if let Some(reload_topic) = &self.reload_topic {
            match self.reload_broker.as_str() {
                "near" => {
                    info!(
                        "Subscribing to reload topic '{}' on NEAR broker",
                        reload_topic
                    );
                    self.near_client
                        .subscribe(reload_topic, QoS::AtLeastOnce)
                        .await
                        .context("Failed to subscribe to reload topic")?;
                }
                "far" => {
                    info!(
                        "Subscribing to reload topic '{}' on FAR broker",
                        reload_topic
                    );
                    self.far_client
                        .subscribe(reload_topic, QoS::AtLeastOnce)
                        .await
                        .context("Failed to subscribe to reload topic")?;
                }
                _ => warn!("Invalid reload_broker value: {}", self.reload_broker),
            }
        }

        Ok(())
    }

    async fn reload_config(&self) -> Result<()> {
        info!("Reloading configuration from {:?}", self.config_path);

        let new_config = BridgeConfig::load_from_file(&self.config_path)
            .context("Failed to reload configuration")?;

        // Unsubscribe from old topics
        let old_config = self.config.read().await;
        for rule in &old_config.rules {
            match rule.direction {
                Direction::NearToFar => {
                    debug!("Unsubscribing from '{}' on NEAR broker", rule.topic);
                    let _ = self.near_client.unsubscribe(&rule.topic).await;
                }
                Direction::FarToNear => {
                    debug!("Unsubscribing from '{}' on FAR broker", rule.topic);
                    let _ = self.far_client.unsubscribe(&rule.topic).await;
                }
                Direction::Wherever => {
                    debug!("Unsubscribing from '{}' on BOTH brokers", rule.topic);
                    let _ = self.near_client.unsubscribe(&rule.topic).await;
                    let _ = self.far_client.unsubscribe(&rule.topic).await;
                }
            }
        }
        drop(old_config);

        // Update config
        *self.config.write().await = new_config;

        // Subscribe to new topics
        self.subscribe_to_topics().await?;

        info!("Configuration reloaded successfully");
        Ok(())
    }

    async fn handle_near_publish(&self, publish: Publish) -> Result<()> {
        let config = self.config.read().await;

        // Check if this is a reload message
        if let Some(reload_topic) = &self.reload_topic {
            if self.reload_broker == "near" && publish.topic == *reload_topic {
                drop(config);
                return self.reload_config().await;
            }
        }

        // Find matching rules for this topic
        for rule in &config.rules {
            if matches_topic(&rule.topic, &publish.topic) {
                match rule.direction {
                    Direction::NearToFar | Direction::Wherever => {
                        if rule.logging {
                            info!(
                                "NEAR→FAR: {} ({} bytes, QoS {:?})",
                                publish.topic,
                                publish.payload.len(),
                                publish.qos
                            );
                            debug!("Payload: {:?}", String::from_utf8_lossy(&publish.payload));
                        }

                        self.far_client
                            .publish(
                                &publish.topic,
                                rule.qos(),
                                publish.retain,
                                publish.payload.clone(),
                            )
                            .await
                            .context(format!(
                                "Failed to forward message to far broker: {}",
                                publish.topic
                            ))?;
                    }
                    Direction::FarToNear => {
                        // Ignore messages from near when rule is FarToNear
                        debug!(
                            "Ignoring message on '{}' from NEAR (rule is FarToNear)",
                            publish.topic
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_far_publish(&self, publish: Publish) -> Result<()> {
        let config = self.config.read().await;

        // Check if this is a reload message
        if let Some(reload_topic) = &self.reload_topic {
            if self.reload_broker == "far" && publish.topic == *reload_topic {
                drop(config);
                return self.reload_config().await;
            }
        }

        // Find matching rules for this topic
        for rule in &config.rules {
            if matches_topic(&rule.topic, &publish.topic) {
                match rule.direction {
                    Direction::FarToNear | Direction::Wherever => {
                        if rule.logging {
                            info!(
                                "FAR→NEAR: {} ({} bytes, QoS {:?})",
                                publish.topic,
                                publish.payload.len(),
                                publish.qos
                            );
                            debug!("Payload: {:?}", String::from_utf8_lossy(&publish.payload));
                        }

                        self.near_client
                            .publish(
                                &publish.topic,
                                rule.qos(),
                                publish.retain,
                                publish.payload.clone(),
                            )
                            .await
                            .context(format!(
                                "Failed to forward message to near broker: {}",
                                publish.topic
                            ))?;
                    }
                    Direction::NearToFar => {
                        // Ignore messages from far when rule is NearToFar
                        debug!(
                            "Ignoring message on '{}' from FAR (rule is NearToFar)",
                            publish.topic
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn run(self, near_eventloop: EventLoop, far_eventloop: EventLoop) -> Result<()> {
        info!("Starting MQTT bridge");

        // Subscribe to all configured topics
        self.subscribe_to_topics().await?;

        info!("Bridge is running");

        // Poll each broker's event loop in its own task. Keeping them
        // independent means a burst on one side (e.g. Zigbee2MQTT dumping its
        // retained topics when we subscribe to `#`) can't stall the other side's
        // poll loop — the failure mode that let the far broker's keepalive lapse
        // and reset the connection.
        let bridge = Arc::new(self);

        let near_task = {
            let bridge = Arc::clone(&bridge);
            let mut eventloop = near_eventloop;
            tokio::spawn(async move {
                loop {
                    match eventloop.poll().await {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            if let Err(e) = bridge.handle_near_publish(publish).await {
                                error!("Error handling NEAR publish: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(packet)) => debug!("NEAR incoming: {:?}", packet),
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            error!("NEAR connection error: {}", e);
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            })
        };

        let far_task = {
            let bridge = Arc::clone(&bridge);
            let mut eventloop = far_eventloop;
            tokio::spawn(async move {
                loop {
                    match eventloop.poll().await {
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            if let Err(e) = bridge.handle_far_publish(publish).await {
                                error!("Error handling FAR publish: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(packet)) => debug!("FAR incoming: {:?}", packet),
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            error!("FAR connection error: {}", e);
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            })
        };

        // Both loop forever; surface it if either task dies unexpectedly.
        tokio::select! {
            r = near_task => r.context("near event-loop task ended")?,
            r = far_task => r.context("far event-loop task ended")?,
        }
        Ok(())
    }
}

fn create_mqtt_client(config: &MqttBrokerConfig, name: &str) -> Result<(AsyncClient, EventLoop)> {
    let mut mqttoptions = MqttOptions::new(&config.client_id, &config.host, config.port);

    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        mqttoptions.set_credentials(username, password);
    }

    mqttoptions.set_keep_alive(Duration::from_secs(30));
    // rumqttc's 10 KiB default drops connections on large retained payloads
    // (e.g. Zigbee2MQTT bridge/definitions). Raise it for both directions.
    mqttoptions.set_max_packet_size(config.max_packet_size, config.max_packet_size);

    info!(
        "Creating {} MQTT client: {}:{} (id: {}, max_packet_size: {})",
        name, config.host, config.port, config.client_id, config.max_packet_size
    );

    // Larger request queue so a retained-message burst doesn't backpressure the
    // forwarding path (default was 100, too small for a full z2m dump).
    Ok(AsyncClient::new(mqttoptions, 1024))
}

/// Check if a message topic matches a subscription topic (with wildcards)
fn matches_topic(subscription: &str, topic: &str) -> bool {
    let sub_parts: Vec<&str> = subscription.split('/').collect();
    let topic_parts: Vec<&str> = topic.split('/').collect();

    if sub_parts.last() == Some(&"#") {
        // Multi-level wildcard
        let sub_prefix = &sub_parts[..sub_parts.len() - 1];
        topic_parts.len() >= sub_prefix.len()
            && sub_prefix
                .iter()
                .zip(topic_parts.iter())
                .all(|(s, t)| *s == "+" || *s == *t)
    } else {
        // Single-level wildcards or exact match
        sub_parts.len() == topic_parts.len()
            && sub_parts
                .iter()
                .zip(topic_parts.iter())
                .all(|(s, t)| *s == "+" || *s == *t)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("mr-bridge starting");
    info!("Config file: {:?}", args.config);
    if let Some(reload_topic) = &args.reload_topic {
        info!(
            "Reload topic: {} (on {} broker)",
            reload_topic, args.reload_broker
        );
    }

    let (bridge, near_eventloop, far_eventloop) = Bridge::new(&args).await?;
    bridge.run(near_eventloop, far_eventloop).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_matching() {
        // Exact matches
        assert!(matches_topic("home/living/temp", "home/living/temp"));
        assert!(!matches_topic("home/living/temp", "home/kitchen/temp"));

        // Single-level wildcard
        assert!(matches_topic("home/+/temp", "home/living/temp"));
        assert!(matches_topic("home/+/temp", "home/kitchen/temp"));
        assert!(!matches_topic("home/+/temp", "home/living/room/temp"));

        // Multi-level wildcard
        assert!(matches_topic("home/#", "home/living/temp"));
        assert!(matches_topic("home/#", "home/kitchen/humidity"));
        assert!(matches_topic("home/#", "home"));
        assert!(matches_topic("#", "any/topic/here"));

        // Combined wildcards
        assert!(matches_topic("home/+/sensor/#", "home/living/sensor/temp"));
        assert!(matches_topic(
            "home/+/sensor/#",
            "home/kitchen/sensor/humidity/value"
        ));
        assert!(!matches_topic("home/+/sensor/#", "home/living/other/temp"));
    }
}
