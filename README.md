# atomic_websocket

![Atomic WebSocket Logo](./assets/logo.svg)

[![Crates.io](https://img.shields.io/crates/v/atomic_websocket.svg)](https://crates.io/crates/atomic_websocket)
[![Documentation](https://docs.rs/atomic_websocket/badge.svg)](https://docs.rs/atomic_websocket)
[![License](https://img.shields.io/crates/l/atomic_websocket.svg)](LICENSE)

A high-level, resilient WebSocket client and server implementation for Rust, built on top of [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite).

## Features

- 🚀 **Simplified Connections**: Streamlined API for WebSocket client and server connections
- 🔄 **Automatic Ping/Pong**: Built-in handling of WebSocket ping/pong messages for connection health monitoring
- 🔁 **Auto-Reconnection**: Client connections automatically attempt to reconnect when interrupted
- 🔍 **Automatic Local Network Discovery**: Built-in scanning to find servers on the same local network without manual configuration
- 🛡️ **Connection Health Monitoring**: Continuous connection status checking with configurable intervals
- 📊 **Connection Status Events**: Subscribe to connection state changes for reactive applications
- 🔌 **Serialization Support**: Built-in support for structured data with Bebop serialization
- 💾 **Database Integration**: Optional persistent storage for connection settings and state

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
atomic_websocket = "0.6.27"
tokio = { version = "1", features = ["full"] }
bebop = "0.3" # For serialization
```

## Quick Start

### WebSocket Client Example

Based on the internal client implementation pattern:

```rust
use atomic_websocket::{
    AtomicWebsocket, 
    server_sender::{ClientOptions, SenderStatus, ServerSenderTrait},
    schema::{ServerConnectInfo, Category},
    common::{get_id, make_response_message},
};
use tokio::sync::mpsc::Receiver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure client options
    let mut client_options = ClientOptions::default();
    client_options.retry_seconds = 2;
    client_options.use_keep_ip = true;
    
    // Initialize database and server sender (implementation details in your project)
    let db = initialize_database().await?;
    let server_sender = initialize_server_sender().await?;
    
    // Create atomic websocket client
    let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
        db.clone(),
        client_options,
        server_sender.clone(),
    ).await;

    // Get status and message receivers
    let status_receiver = atomic_client.get_status_receiver().await;
    let handle_message_receiver = atomic_client.get_handle_message_receiver().await;

    // Handle status updates and incoming messages
    tokio::spawn(receive_status(status_receiver));
    tokio::spawn(receive_handle_message(handle_message_receiver));

    // Connect to internal server (can specify server info or use auto-discovery)
    let _ = atomic_client
        .get_internal_connect(
            Some(ServerConnectInfo {
                server_ip: "",
                port: "9000",
            }),
            db.clone(),
        )
        .await;
        
    // Keep application running
    tokio::signal::ctrl_c().await?;
    
    Ok(())
}

// Handle connection status changes
async fn receive_status(mut receiver: Receiver<SenderStatus>) {
    while let Some(status) = receiver.recv().await {
        println!("Connection status: {:?}", status);
        
        if status == SenderStatus::Connected {
            println!("Connected to server!");
            
            // Example: Send initialization message upon connection
            // let id = get_id(db.clone()).await;
            // server_sender()
            //     .send(make_response_message(
            //         Category::AppStartup,
            //         serialized_data,
            //     ))
            //     .await;
        }
    }
}

// Handle incoming messages
async fn receive_handle_message(mut receiver: Receiver<Vec<u8>>) {
    while let Some(message) = receiver.recv().await {
        println!("Received message: {} bytes", message.len());
        
        // Process incoming messages based on category
        // if let Ok(data) = Data::deserialize(&message) {
        //     match Category::try_from(data.category as u32).unwrap() {
        //         Category::YourCategory => {
        //             // Handle specific message type
        //         },
        //         _ => println!("Unknown message category"),
        //     }
        // }
    }
}
```

### WebSocket Server Example

Based on the internal server implementation pattern:

```rust
use atomic_websocket::{
    AtomicWebsocket,
    client_sender::{ClientSenders, ClientSendersTrait, ServerOptions},
    schema::{Category, Data},
    common::make_response_message,
};
use tokio::sync::mpsc::Receiver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure server options
    let options = ServerOptions::default();
    
    // Initialize client senders (implementation details in your project)
    let client_senders = initialize_client_senders().await?;
    
    // Create atomic websocket server
    let address = "0.0.0.0:9000";
    println!("Starting server on {}", address);
    
    let atomic_server = AtomicWebsocket::get_internal_server_with_client_senders(
        address.to_string(),
        options,
        client_senders.clone(),
    ).await;
    
    // Get message receiver
    let handle_message_receiver = atomic_server.get_handle_message_receiver().await;
    
    // Handle incoming messages
    tokio::spawn(receive_server_messages(handle_message_receiver));
    
    // Keep server running
    tokio::signal::ctrl_c().await?;
    
    Ok(())
}

// Handle incoming server messages
async fn receive_server_messages(mut receiver: Receiver<(Vec<u8>, String)>) {
    while let Some((message, peer)) = receiver.recv().await {
        println!("Received message from {}: {} bytes", peer, message.len());
        
        // Process incoming messages based on category
        if let Ok(data) = Data::deserialize(&message) {
            match Category::try_from(data.category as u32).unwrap_or_default() {
                Category::AppStartup => {
                    println!("Received AppStartup from {}", peer);
                    
                    // Example: Send response back to specific client
                    // let response_data = serialize_your_response();
                    // client_senders()
                    //     .send(
                    //         &peer,
                    //         make_response_message(Category::AppStartupOutput, response_data),
                    //     )
                    //     .await;
                },
                _ => println!("Unknown message category from {}", peer),
            }
        }
    }
}
```

## Real-World Implementation Example

### Client Implementation with Auto-Reconnect

```rust
use atomic_websocket::{
    AtomicWebsocket,
    server_sender::{ClientOptions, SenderStatus, ServerSender, ServerSenderTrait},
    schema::{AppStartup, AppStartupOutput, Category, Data, ServerConnectInfo},
    common::{get_id, make_response_message},
};
use bebop::Record;
use tokio::time::sleep;
use std::time::Duration;

async fn start_client(port: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Configure client with auto-reconnect
    let mut client_options = ClientOptions::default();
    client_options.retry_seconds = 2;  // Retry connection every 2 seconds
    client_options.use_keep_ip = true; // Remember last successful IP
    
    // Get atomic client instance
    let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
        db().clone(),
        client_options,
        server_sender().clone(),
    ).await;

    // Get status and message receivers
    let status_receiver = atomic_client.get_status_receiver().await;
    let handle_message_receiver = atomic_client.get_handle_message_receiver().await;

    // Handle connection status changes
    tokio::spawn(async move {
        while let Some(status) = status_receiver.recv().await {
            println!("Status: {:?}", status);
            
            if status == SenderStatus::Connected {
                println!("Connected to server!");
                
                // Send application startup message
                let id = get_id(db().clone()).await;
                let mut data = vec![];
                AppStartup {
                    id: &id,
                    app_type: 1,
                }.serialize(&mut data).unwrap();
                
                server_sender()
                    .send(make_response_message(
                        Category::AppStartup,
                        data,
                    ))
                    .await;
            }
        }
    });

    // Handle incoming messages
    tokio::spawn(async move {
        while let Some(message) = handle_message_receiver.recv().await {
            if let Ok(data) = Data::deserialize(&message) {
                match Category::try_from(data.category as u32).unwrap() {
                    Category::AppStartupOutput => {
                        println!("Received startup confirmation: {:?}", 
                            AppStartupOutput::deserialize(&data.datas).unwrap()
                        );
                        
                        // Send periodic keep-alive messages
                        sleep(Duration::from_secs(2)).await;
                        let id = get_id(db().clone()).await;
                        let mut data = vec![];
                        AppStartup {
                            id: &id,
                            app_type: 1,
                        }.serialize(&mut data).unwrap();
                        
                        server_sender()
                            .send(make_response_message(
                                Category::AppStartup,
                                data,
                            ))
                            .await;
                    },
                    _ => println!("Unknown message category"),
                }
            }
        }
    });

    // Connect to server (with auto-reconnect if connection fails)
    let connection_result = atomic_client
        .get_internal_connect(
            Some(ServerConnectInfo {
                server_ip: "",
                port,
            }),
            db().clone(),
        )
        .await;
        
    println!("Connection initiated: {:?}", connection_result);
    
    Ok(())
}
```

### Server Implementation with Client Management

```rust
use atomic_websocket::{
    AtomicWebsocket,
    client_sender::{ClientSenders, ClientSendersTrait, ServerOptions},
    schema::{AppStartup, AppStartupOutput, Category, Data},
    common::make_response_message,
};
use bebop::Record;

async fn start_server(address: String) -> Result<(), Box<dyn std::error::Error>> {
    // Create server with default options
    let options = ServerOptions::default();
    
    // Initialize server with client senders for managing connections
    let atomic_server = AtomicWebsocket::get_internal_server_with_client_senders(
        address,
        options,
        client_senders().clone(),
    ).await;
    
    // Get message receiver
    let handle_message_receiver = atomic_server.get_handle_message_receiver().await;
    
    // Handle incoming messages
    tokio::spawn(async move {
        while let Some((data, peer)) = handle_message_receiver.recv().await {
            if let Ok(message) = Data::deserialize(&data) {
                match Category::try_from(message.category as u32).unwrap() {
                    Category::AppStartup => {
                        println!("Client {} started up", peer);
                        println!(
                            "Startup details: {:?}",
                            AppStartup::deserialize(&message.datas).unwrap()
                        );
                        
                        // Send confirmation back to client
                        let mut response_data = vec![];
                        AppStartupOutput { success: true }
                            .serialize(&mut response_data)
                            .unwrap();
                            
                        client_senders()
                            .send(
                                &peer,
                                make_response_message(Category::AppStartupOutput, response_data),
                            )
                            .await;
                    },
                    _ => println!("Unknown message from {}: {:?}", peer, message),
                }
            }
        }
    });
    
    println!("Server started on {}", address);
    Ok(())
}
```

## Features in Detail

### Local Network Discovery

The library provides built-in automatic server discovery on the local network. This functionality is handled internally by the `ScanManager` when you use the internal client connection:

```rust
// When connecting to an internal server without specifying an IP
// The system will automatically scan the local network for available servers
let atomic_client = AtomicWebsocket::get_internal_client_with_server_sender(
    db.clone(),
    client_options,
    server_sender.clone(),
).await;

// Connect with just a port - server discovery happens automatically
let result = atomic_client
    .get_internal_connect(
        Some(ServerConnectInfo {
            server_ip: "",  // Empty server_ip triggers local network scanning
            port: "9000",
        }),
        db.clone(),
    )
    .await;
```

When `server_ip` is empty, the library automatically scans the local network for servers on the specified port. This is handled by the internal `ScanManager` which takes care of UDP broadcasts and response handling to find available servers.

> **Note:** Internet connectivity is required for the automatic network scanning to function properly. The scanning process uses an internet connection to determine the local network configuration.

### Automatic Reconnection

The client will automatically attempt to reconnect when the connection is lost:

```rust
let mut client_options = ClientOptions::default();
client_options.retry_seconds = 5;       // Retry every 5 seconds
client_options.max_retry_count = 10;    // Maximum 10 retry attempts
client_options.use_keep_ip = true;      // Remember the last working IP
```

### Connection Status Monitoring

Monitor the health and status of your WebSocket connections:

```rust
let status_receiver = client.get_status_receiver().await;

tokio::spawn(async move {
    while let Some(status) = status_receiver.recv().await {
        match status {
            SenderStatus::Start => println!("First start to connect..."),
            SenderStatus::Connected => println!("Connection established"),
            SenderStatus::Disconnected => println!("Connection lost"),
        }
    }
});
```

## Comparison with tokio-tungstenite

While `tokio-tungstenite` provides a robust low-level WebSocket implementation, `atomic_websocket` enhances it with reliability features:

| Feature | tokio-tungstenite | atomic_websocket |
|---------|-------------------|-----------------|
| Connection API | Low-level, manual | High-level, simplified |
| Ping/Pong | Manual implementation | Automatic handling |
| Reconnection | Not included | Automatic with configurable retry |
| Local Network Scanning | Not included | Built-in automatic discovery |
| Connection Status | Manual tracking | Built-in monitoring and events |
| Message Serialization | Manual encoding | Built-in with Bebop support |
| Connection Health | Manual checks | Automatic monitoring |
| Database Integration | Not included | Optional persistent storage |

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite) - The foundation for this library
- [tokio](https://tokio.rs/) - The async runtime powering this library
- [bebop](https://github.com/betwixt-labs/bebop) - For efficient binary serialization