# Changes

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