use anyhow::{Context, Result};
use clap::Parser;
use mr_bridge::{Args, BridgeConfig, Direction, MqttBrokerConfig};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, Publish, QoS};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

struct Bridge {
    near_client: AsyncClient,
    far_client: AsyncClient,
    config: Arc<RwLock<BridgeConfig>>,
    config_path: std::path::PathBuf,
    reload_topic: Option<String>,
    reload_broker: String,
    /// Fingerprints of recently-forwarded messages, for echo suppression. Empty
    /// / unused when `dedup_window` is zero. Guarded by a plain mutex — only ever
    /// held for the duration of a hash-map op, never across an `.await`.
    dedup: Mutex<HashMap<u64, Instant>>,
    dedup_window: Duration,
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
            config_path: args.config.clone(),
            reload_topic: args.reload_topic.clone(),
            reload_broker: args.reload_broker.clone(),
            dedup: Mutex::new(HashMap::new()),
            dedup_window: Duration::from_secs(config.dedup_window_secs),
            config: Arc::new(RwLock::new(config)),
        };
        Ok((bridge, near_eventloop, far_eventloop))
    }

    /// Fingerprint a message by topic + payload.
    fn fingerprint(topic: &str, payload: &[u8]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        topic.hash(&mut h);
        payload.hash(&mut h);
        h.finish()
    }

    /// Record that we just forwarded this message, so its echo can be recognised.
    /// No-op when loop prevention is disabled.
    fn mark_forwarded(&self, topic: &str, payload: &[u8]) {
        if self.dedup_window.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut cache = self.dedup.lock().unwrap();
        // Opportunistically drop expired entries when the map grows.
        if cache.len() > 8192 {
            cache.retain(|_, t| now.duration_since(*t) < self.dedup_window);
        }
        cache.insert(Self::fingerprint(topic, payload), now);
    }

    /// True if this message matches one we forwarded within the window — i.e.
    /// it's the echo of our own forward bouncing back, and must not be forwarded
    /// again (that's the loop). Always false when loop prevention is disabled.
    fn is_echo(&self, topic: &str, payload: &[u8]) -> bool {
        if self.dedup_window.is_zero() {
            return false;
        }
        let fp = Self::fingerprint(topic, payload);
        let cache = self.dedup.lock().unwrap();
        matches!(cache.get(&fp), Some(t) if t.elapsed() < self.dedup_window)
    }

    /// Subscribe the NEAR client to every topic it should receive (rules going
    /// near→far or both ways, plus the reload topic if it lives on near). Called
    /// on every (re)connect, since rumqttc does not replay subscriptions after a
    /// dropped connection — without this the bridge reconnects but goes silent.
    async fn subscribe_near(&self) -> Result<()> {
        let config = self.config.read().await;
        for rule in &config.rules {
            if matches!(rule.direction, Direction::NearToFar | Direction::Wherever) {
                info!("Subscribing NEAR to '{}'", rule.topic);
                self.near_client
                    .subscribe(&rule.topic, rule.qos())
                    .await
                    .context(format!("subscribe near '{}'", rule.topic))?;
            }
        }
        if let Some(reload_topic) = &self.reload_topic {
            if self.reload_broker == "near" {
                self.near_client
                    .subscribe(reload_topic, QoS::AtLeastOnce)
                    .await
                    .context("subscribe near reload topic")?;
            }
        }
        Ok(())
    }

    /// Subscribe the FAR client to every topic it should receive (rules going
    /// far→near or both ways, plus the reload topic if it lives on far).
    async fn subscribe_far(&self) -> Result<()> {
        let config = self.config.read().await;
        for rule in &config.rules {
            if matches!(rule.direction, Direction::FarToNear | Direction::Wherever) {
                info!("Subscribing FAR to '{}'", rule.topic);
                self.far_client
                    .subscribe(&rule.topic, rule.qos())
                    .await
                    .context(format!("subscribe far '{}'", rule.topic))?;
            }
        }
        if let Some(reload_topic) = &self.reload_topic {
            if self.reload_broker == "far" {
                self.far_client
                    .subscribe(reload_topic, QoS::AtLeastOnce)
                    .await
                    .context("subscribe far reload topic")?;
            }
        }
        Ok(())
    }

    /// Re-subscribe both brokers (used on config reload).
    async fn subscribe_to_topics(&self) -> Result<()> {
        self.subscribe_near().await?;
        self.subscribe_far().await?;
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

        // Loop prevention: if this is the echo of something we just forwarded
        // FAR→NEAR, don't forward it back to FAR.
        if self.is_echo(&publish.topic, &publish.payload) {
            debug!("NEAR: dropping echoed message: {}", publish.topic);
            return Ok(());
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

                        self.mark_forwarded(&publish.topic, &publish.payload);
                        // Non-blocking: if the FAR queue is full (far broker
                        // unreachable) we drop rather than block, so a FAR outage
                        // can't stall — and thereby kill — the NEAR connection.
                        // Current state is re-sent once FAR is back and publishing
                        // resumes.
                        if let Err(e) = self.far_client.try_publish(
                            &publish.topic,
                            rule.qos(),
                            publish.retain,
                            publish.payload.clone(),
                        ) {
                            debug!("dropping NEAR→FAR '{}' (far unavailable): {}", publish.topic, e);
                        }
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

        // Loop prevention: if this is the echo of something we just forwarded
        // NEAR→FAR, don't forward it back to NEAR.
        if self.is_echo(&publish.topic, &publish.payload) {
            debug!("FAR: dropping echoed message: {}", publish.topic);
            return Ok(());
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

                        self.mark_forwarded(&publish.topic, &publish.payload);
                        // Non-blocking (see handle_near_publish): drop rather than
                        // block the FAR loop if the NEAR queue is full.
                        if let Err(e) = self.near_client.try_publish(
                            &publish.topic,
                            rule.qos(),
                            publish.retain,
                            publish.payload.clone(),
                        ) {
                            debug!("dropping FAR→NEAR '{}' (near unavailable): {}", publish.topic, e);
                        }
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
        info!("Bridge is running");

        // Poll each broker's event loop in its own task. Keeping them independent
        // means a burst or an outage on one side can't stall the other's poll
        // loop. Each task (re)subscribes on every ConnAck so it recovers cleanly
        // from a dropped connection, and logs a lost connection only once per
        // outage instead of on every 5s retry.
        let bridge = Arc::new(self);

        let near_task = {
            let bridge = Arc::clone(&bridge);
            let mut eventloop = near_eventloop;
            tokio::spawn(async move {
                let mut reported_error = false;
                loop {
                    match eventloop.poll().await {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            info!("NEAR connected — subscribing");
                            reported_error = false;
                            if let Err(e) = bridge.subscribe_near().await {
                                error!("NEAR subscribe failed: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            if let Err(e) = bridge.handle_near_publish(publish).await {
                                error!("Error handling NEAR publish: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(packet)) => debug!("NEAR incoming: {:?}", packet),
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            if !reported_error {
                                warn!("NEAR connection lost ({}) — retrying every 5s", e);
                                reported_error = true;
                            }
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
                let mut reported_error = false;
                loop {
                    match eventloop.poll().await {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            info!("FAR connected — subscribing");
                            reported_error = false;
                            if let Err(e) = bridge.subscribe_far().await {
                                error!("FAR subscribe failed: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(Packet::Publish(publish))) => {
                            if let Err(e) = bridge.handle_far_publish(publish).await {
                                error!("Error handling FAR publish: {:#}", e);
                            }
                        }
                        Ok(Event::Incoming(packet)) => debug!("FAR incoming: {:?}", packet),
                        Ok(Event::Outgoing(_)) => {}
                        Err(e) => {
                            if !reported_error {
                                warn!("FAR connection lost ({}) — retrying every 5s", e);
                                reported_error = true;
                            }
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

    mqttoptions.set_keep_alive(Duration::from_secs(config.keepalive_secs));
    // rumqttc's 10 KiB default drops connections on large retained payloads
    // (e.g. Zigbee2MQTT bridge/definitions). Raise it for both directions.
    mqttoptions.set_max_packet_size(config.max_packet_size, config.max_packet_size);

    info!(
        "Creating {} MQTT client: {}:{} (id: {}, keepalive: {}s, max_packet_size: {})",
        name, config.host, config.port, config.client_id, config.keepalive_secs, config.max_packet_size
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
    fn fingerprint_is_stable_and_sensitive() {
        let base = Bridge::fingerprint("zigbee2mqtt/light", b"{\"state\":\"ON\"}");
        assert_eq!(
            base,
            Bridge::fingerprint("zigbee2mqtt/light", b"{\"state\":\"ON\"}")
        );
        assert_ne!(
            base,
            Bridge::fingerprint("zigbee2mqtt/other", b"{\"state\":\"ON\"}")
        );
        assert_ne!(
            base,
            Bridge::fingerprint("zigbee2mqtt/light", b"{\"state\":\"OFF\"}")
        );
    }

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
