# Changes

## 0.6.24

* Fixed release connect when duplicated connect logic.

## 0.6.23

* Fixed release connect when error in connect logic.

## 0.6.22

* Fixed prevent retry connect when connected to server.

## 0.6.21

* Fixed prevent retry connect when connecting progress.

## 0.6.20

* Improve client connection stability when write received time with message from server.

## 0.6.19

* Improve client scan logic.

## 0.6.18

* Improve client keepalive connect when no server_ip.

## 0.6.17

* Improve client keepalive connect.

## 0.6.16

* Fixed prevent duplicate client reconnection from same IP

## 0.6.15

* Change dependency: Replace OpenSSL with Rustls

## 0.6.14

* Improve atomic type of db, refactoring types.

## 0.6.13

* Update dependencies.

## 0.6.12

* Update dependencies.

## 0.6.11

* Rollback to 0.6.6v.

## 0.6.10

* Remove for server_sender when process dropped.

## 0.6.9

* Fixed for server_sender when process dropped in try_connect condition.

## 0.6.8

* Fixed for server_sender when process dropped in loop checker.

## 0.6.7

* Add for server_sender when process dropped.

## 0.6.6

* Fixed for add server_ip.

## 0.6.5

* Improve scan ip using timeout(10s).

## 0.6.4

* Fixed for client_sender's handle message.

## 0.6.3

* Fixed for scan_manager when is_connected function.

## 0.6.2

* Fixed scan ip for internal client connect when connected server.

## 0.6.1

* Improve scan ip for internal client connect, change server handle message receiver from broadcast to mpsc.

## 0.6.0

* Improve scan ip for internal client connect, adjust backoff send, change message receiver from broadcast to mpsc.

## 0.5.8

* Fixed for client_sender's check_client_send_time.

## 0.5.7

* Remove for server_sender's get_server_ip, fixed get_data_schema.

## 0.5.6

* Fixed for server_sender's change_ip to remove_ip.

## 0.5.5

* Add for client_connector timeout option.

## 0.5.4

* Add for client_senders's send is return bool.

## 0.5.3

* Fixed for freeze client_sender when client_senders send fail process.

## 0.5.2

* Fixed for remove client when client_senders send fail.

## 0.5.1

* Add make_atomic_message and prevent call when server_ip found. 

## 0.5.0

* Convert import message for any language.

## 0.4.5

* Add import binary from all message.

## 0.4.4

* Add import binary from text message.

## 0.4.3

* Fixed duplicated connect and infinite connection, improve logic.

## 0.4.2

* Add is_active for clientSender.

## 0.4.1

* Improve loop and db logic.

## 0.4.0

* Add client and server with senders

## 0.3.18

* Update dependencies.

## 0.3.17

* Add function is get_ip_address.

## 0.3.16

* Fixed for connect server_ip.

## 0.3.15

* Fixed for connect state condition to write received message times.

## 0.3.14

* Update dependencies.

## 0.3.13

* Fixed for is_avalid_server_ip condition.

## 0.3.12

* Add disconnect for client when duplicate connector.

## 0.3.11

* Change server message channel. 4 -> 1024

## 0.3.10

* Change communicate message to async.

## 0.3.9

* Add rinf debug option.

## 0.3.8

* Change server sender to improve response.

## 0.3.7

* Change refactor loop client, split test in client and server.

## 0.3.6

* Add for client ping delay using retry_seconds option.

## 0.3.5

* Fixed for client loopChecker when send message locking.

## 0.3.4

* Fixed for client loopChecker when deadlock.

## 0.3.3

* Fixed for client loopChecker when read and write.

## 0.3.2

* Add condition for is_valid_server_ip(calculate server_send_times).

## 0.3.1

* Add use_keep_ip option for client.

## 0.3.0

* Change for watch receiver to broadcast receiver! 

## 0.2.7

* Fixed for client when received pong with connectState.

## 0.2.6

* Add retry_seconds option for client.(default: 30 seconds)

## 0.2.5

* Fixed for native_tls logger.

## 0.2.4

* Change for logging system. Thank for Jake Kwak!
  And Fixed init receive handle_message error.

## 0.2.3

* Add for server proxy ping option.

## 0.2.2

* Fixed for server acceptor with spawn.

## 0.2.1

* Fixed for native_tls connector missing dependency. 

## 0.2.0

* Add outer connect client. 

## 0.1.10

* Fixed for default value in watch message handle.

## 0.1.9

* Add options for client and server. ( use_ping )

## 0.1.8

* Add clone for Settings struct, remove export serde in external.

## 0.1.7

* Add export external list.

  async_trait,
  nanoid,
  serde,
  tokio

## 0.1.6

* Add partialEqual for SenderStatus.

## 0.1.5

* Add get_connect for client.

## 0.1.4

* Add get_server_ip for ServerSender.

## 0.1.3

* Add export get_id.

## 0.1.2

* Reorder export list for client and server.

## 0.1.1

* Add re export for client and server.

## 0.1.0

* First init simple websocket.