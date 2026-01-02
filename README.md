# MR-BRIDGE

MR-Bridge - A flexible, configurable tool for bridging topics between two MQTT brokers with dynamic configuration reloading.

## Features

- ✅ **Bidirectional Bridging**: Forward messages between two MQTT brokers in any direction
- ✅ **Flexible Topic Rules**: Support for MQTT wildcards (`+` for single-level, `#` for multi-level)
- ✅ **Dynamic Configuration Reload**: Update bridge rules on-the-fly by publishing to a reload topic
- ✅ **Configurable QoS**: Set Quality of Service per-rule (0, 1, or 2)
- ✅ **Optional Logging**: Log individual messages per-rule for debugging
- ✅ **Auto-reconnection**: Automatic reconnection on connection failures
- ✅ **Multiple Formats**: Support for TOML and JSON configuration files

## Installation

### From Source

```bash
git clone <repository-url>
cd mr-bridge
cargo build --release
```

The binary will be available at `target/release/mr-bridge`.

### Using Docker

Pre-built Docker images are available from GitHub Container Registry:

```bash
# Pull the latest image
docker pull ghcr.io/<username>/mr-bridge:latest

# Run with a config file mounted
docker run -v $(pwd)/config.toml:/app/config.toml \
  ghcr.io/<username>/mr-bridge:latest \
  --config /app/config.toml

# Run with environment variable
docker run -e MR_BRIDGE_CONFIG=/app/config.toml \
  -v $(pwd)/config.toml:/app/config.toml \
  ghcr.io/<username>/mr-bridge:latest
```

### Building Docker Image Locally

```bash
# Build the image
docker build -t mr-bridge .

# Run the image
docker run -v $(pwd)/config.toml:/app/config.toml \
  mr-bridge --config /app/config.toml
```

## Configuration

### Configuration File

Create a configuration file in TOML or JSON format. See `config.example.toml` or `config.example.json` for examples.

#### TOML Format

```toml
[near]
host = "localhost"
port = 1883
username = "user1"  # optional
password = "pass1"  # optional
client_id = "mr-bridge-near"  # optional, auto-generated if not provided

[far]
host = "mqtt.example.com"
port = 1883

[[rules]]
topic = "sensors/#"
direction = "near_to_far"
logging = true
qos = 1

[[rules]]
topic = "commands/#"
direction = "far_to_near"
logging = false
qos = 1

[[rules]]
topic = "status/+"
direction = "wherever"  # bidirectional
logging = false
qos = 0
```

#### JSON Format

```json
{
  "near": {
    "host": "localhost",
    "port": 1883
  },
  "far": {
    "host": "mqtt.example.com",
    "port": 1883
  },
  "rules": [
    {
      "topic": "sensors/#",
      "direction": "near_to_far",
      "logging": true,
      "qos": 1
    }
  ]
}
```

### Configuration Options

#### Broker Configuration (`near` and `far`)

- **`host`** (required): MQTT broker hostname or IP address
- **`port`** (optional, default: 1883): MQTT broker port
- **`username`** (optional): Username for authentication
- **`password`** (optional): Password for authentication
- **`client_id`** (optional): MQTT client ID (auto-generated UUID if not specified)

#### Bridge Rules (`rules`)

- **`topic`** (required): MQTT topic pattern with wildcard support
  - `+` matches a single level (e.g., `home/+/temp` matches `home/kitchen/temp`)
  - `#` matches multiple levels (e.g., `sensors/#` matches `sensors/temp`, `sensors/room/temp`)
- **`direction`** (required): Bridge direction
  - `near_to_far`: Forward messages from near broker to far broker
  - `far_to_near`: Forward messages from far broker to near broker
  - `wherever`: Bidirectional bridging (forward in both directions)
- **`logging`** (optional, default: false): Log each bridged message
- **`qos`** (optional, default: 0): Quality of Service level (0, 1, or 2)

## Usage

### Basic Usage

```bash
# Using a TOML configuration file
mr-bridge --config config.toml

# Using a JSON configuration file
mr-bridge --config config.json

# Using environment variable
export MR_BRIDGE_CONFIG=config.toml
mr-bridge
```

### Dynamic Configuration Reload

Enable dynamic reloading by specifying a reload topic:

```bash
# Reload when a message is published to 'admin/reload' on the near broker
mr-bridge --config config.toml --reload-topic admin/reload

# Listen for reload on the far broker instead
mr-bridge --config config.toml --reload-topic admin/reload --reload-broker far
```

To trigger a reload, publish any message to the reload topic:

```bash
mosquitto_pub -t admin/reload -m "reload"
```

The bridge will:
1. Load the updated configuration file
2. Unsubscribe from old topics
3. Subscribe to new topics
4. Continue operating without disconnecting from brokers

### Environment Variables

All CLI arguments can be set via environment variables:

- `MR_BRIDGE_CONFIG`: Path to configuration file
- `MR_BRIDGE_RELOAD_TOPIC`: Reload topic
- `MR_BRIDGE_RELOAD_BROKER`: Broker to listen for reload messages (`near` or `far`)

### Logging

Control log verbosity with the `RUST_LOG` environment variable:

```bash
# Info level (default)
export RUST_LOG=info
mr-bridge --config config.toml

# Debug level for troubleshooting
export RUST_LOG=debug
mr-bridge --config config.toml

# Only errors
export RUST_LOG=error
mr-bridge --config config.toml
```

## Examples

### Example 1: Forward IoT Sensor Data to Cloud

Forward all sensor data from local MQTT broker to cloud broker:

```toml
[near]
host = "localhost"
port = 1883

[far]
host = "cloud.mqtt.provider.com"
port = 8883
username = "cloud_user"
password = "cloud_password"

[[rules]]
topic = "sensors/#"
direction = "near_to_far"
logging = true
qos = 1
```

### Example 2: Receive Commands from Cloud

Receive commands from cloud and forward to local devices:

```toml
[near]
host = "localhost"
port = 1883

[far]
host = "cloud.mqtt.provider.com"
port = 8883
username = "cloud_user"
password = "cloud_password"

[[rules]]
topic = "commands/#"
direction = "far_to_near"
logging = true
qos = 1
```

### Example 3: Bidirectional Sync

Keep certain topics synchronized between brokers:

```toml
[near]
host = "broker1.local"
port = 1883

[far]
host = "broker2.local"
port = 1883

[[rules]]
topic = "shared/#"
direction = "wherever"
logging = false
qos = 1
```

### Example 4: Complex Multi-Rule Bridge

Different rules for different topic patterns:

```toml
[near]
host = "localhost"
port = 1883

[far]
host = "remote.example.com"
port = 1883

# Forward all temperature readings
[[rules]]
topic = "home/+/temperature"
direction = "near_to_far"
logging = true
qos = 0

# Forward all humidity readings
[[rules]]
topic = "home/+/humidity"
direction = "near_to_far"
logging = true
qos = 0

# Receive control commands
[[rules]]
topic = "home/+/control"
direction = "far_to_near"
logging = true
qos = 1

# Bidirectional status updates
[[rules]]
topic = "home/+/status"
direction = "wherever"
logging = false
qos = 0
```

## How It Works

1. **Startup**: mr-bridge connects to both MQTT brokers (near and far)
2. **Subscription**: Subscribes to all topics defined in rules based on direction
3. **Message Handling**: When a message arrives:
   - Checks if it matches any configured topic patterns
   - Verifies the direction matches
   - Forwards the message to the target broker
   - Optionally logs the message
4. **Dynamic Reload**: When a message is received on the reload topic:
   - Reloads configuration file
   - Unsubscribes from old topics
   - Subscribes to new topics
   - Continues operation seamlessly

## Testing

Run the test suite:

```bash
cargo test
```

### Manual Testing with Mosquitto

1. Start two MQTT brokers:
```bash
# Terminal 1 - Near broker
mosquitto -p 1883

# Terminal 2 - Far broker
mosquitto -p 1884
```

2. Create a test config:
```toml
[near]
host = "localhost"
port = 1883

[far]
host = "localhost"
port = 1884

[[rules]]
topic = "test/#"
direction = "near_to_far"
logging = true
qos = 1
```

3. Run the bridge:
```bash
mr-bridge --config test.toml
```

4. Test message forwarding:
```bash
# Subscribe to far broker
mosquitto_sub -p 1884 -t "test/#"

# Publish to near broker (in another terminal)
mosquitto_pub -p 1883 -t "test/message" -m "Hello from near!"
```

You should see the message appear on the far broker.

## Architecture

- **Async Runtime**: Built on Tokio for efficient concurrent operation
- **MQTT Client**: Uses `rumqttc` for robust MQTT 3.1.1 support
- **Configuration**: Supports both TOML and JSON via serde
- **Error Handling**: Comprehensive error handling with automatic reconnection
- **Topic Matching**: Custom wildcard matching implementation for MQTT topics

## CI/CD

This project uses GitHub Actions for continuous integration and deployment:

### Automated Checks

On every push and pull request:
- **Formatting**: `cargo fmt` verifies code formatting
- **Linting**: `cargo clippy` checks for common mistakes and improvements
- **Tests**: `cargo test` runs the test suite

### Docker Image Publishing

On pushes to `main` branch:
- Multi-architecture Docker images (amd64, arm64) are built and pushed to GitHub Container Registry
- Images are tagged with:
  - `latest` - Latest stable version from main branch
  - `<branch>-<sha>` - Commit-specific tags
  - `v*` - Semantic version tags (for git tags)

### Using Published Images

```bash
# Pull the latest stable image
docker pull ghcr.io/<username>/mr-bridge:latest

# Pull a specific commit
docker pull ghcr.io/<username>/mr-bridge:main-abc1234

# Pull a tagged release
docker pull ghcr.io/<username>/mr-bridge:v1.0.0
```

### Docker Compose Example

```yaml
version: '3.8'

services:
  mr-bridge:
    image: ghcr.io/<username>/mr-bridge:latest
    container_name: mr-bridge
    restart: unless-stopped
    volumes:
      - ./config.toml:/app/config.toml:ro
    environment:
      - MR_BRIDGE_CONFIG=/app/config.toml
      - RUST_LOG=info
    # Optional: If you need to access local MQTT brokers
    network_mode: host
```

## Troubleshooting

### Connection Issues

If you can't connect to a broker:
- Verify the host and port are correct
- Check if authentication is required
- Ensure firewall rules allow the connection
- Check broker logs for connection attempts

### Messages Not Forwarding

If messages aren't being bridged:
- Verify the topic pattern matches (wildcards are case-sensitive)
- Check the direction is correct
- Enable logging on the rule to see if messages are being received
- Use `RUST_LOG=debug` to see detailed internal operations

### Configuration Reload Not Working

If reload isn't working:
- Verify the reload topic is correct
- Check which broker you're publishing to (matches `--reload-broker`)
- Ensure the configuration file is readable and valid
- Check logs for reload errors

## License

[Add your license here]

## Contributing

[Add contribution guidelines here]
