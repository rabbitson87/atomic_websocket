# Changes

## 0.9.9

### Fixed

- A client could have its new connection closed by the server the moment it
  was made. The server learns who a client is from the first frame and closes
  a connection that starts with anything else; the client registered a new
  connection and only then queued its first ping, so an application sending
  at that moment got its message out first. An application with messages
  waiting for the link to come back sends exactly then. The ping is now
  queued before the connection is registered, so it is always the first
  frame. The ping scheduled after a pong goes only to the connection whose
  pong it was, not to one that has replaced it.

- A clock set back made a dead link look alive. Liveness was read from
  wall-clock stamps: set back, the stamp of the last message lay in the
  clock's future and read as "just now" until the clock caught up, so for as
  long as it had been set back `is_valid_server_ip` said yes and the checker
  never called the link dead. Both now also read a monotonic clock and take
  the longer of the two silences. `server_received_times` and `connected_at`
  stay as they were (wall clock, whole seconds), for display and for callers
  that move them back to stand for silence; a clock set forward still makes
  the checker redial once, as before.

- The server answered a ping by sending the pong to whichever connection had
  the client's id, not on the connection the ping came in on. After a
  reconnect, the old connection's last pings (still read by the server)
  were answered on the new one. Harmless until a client counts its pongs
  (see `delivered` below): it would then count answers to pings it never
  sent on that connection. The server now answers on the connection itself.
  A client of an older server can still see such a stray pong after a
  reconnect. As before, a pong that goes out marks the client heard from
  (a client that only pings stays in the table) and counts in the metrics,
  and one that cannot be queued drops the client, but only while the
  client's id is still registered with that connection. The old
  connection's last pings are no longer answered on the new one.

- The client read websocket ping, pong, close and text frames as the
  protocol's messages when their payload began like one (a text frame could
  pass for a pong, or for a goodbye). It now reads binary frames only; any
  frame still counts as the server being heard from.

- A subnet scan could miss a connection made in the same second as the scan
  started, and run on to open a second one. It watched `connected_at`, which
  two connections made in one second share; it now watches the connection
  number.

### Added

- `ServerSender::connection`: the number of the registered connection, 1 for
  the first, never reused while the process runs.
- `ServerSenderTrait::send_to(message, connection) -> bool`: sends on that
  connection only, and says whether its queue took the message. `send` still
  returns nothing; it is `send_to` on whatever connection is registered.
- `ServerSender::heard_ago`, `registered_ago` and `mark_heard`: the readings
  the checker uses, and the way to record a message.
- `ServerSender::delivery_mark(connection)` and `delivered(connection,
  mark)`: whether the server has read a message nothing answers. The client
  counts the pings it queues on each connection (before queueing them) and
  the pongs that come back; read the mark right after `send_to` took the
  message, and once more pongs than that have come back on the same
  connection, one answers a ping queued after the message, so the server
  read the message. Never true for a connection that has been replaced.
  Exact against a 0.9.9 server; an older one can send a stray pong after a
  reconnect (above).
- `send_to` is a new required method of `ServerSenderTrait`. Nothing outside
  the crate is known to implement the trait; an implementation that does has
  to add it.
- The server lets a client go after `client_timeout_seconds` of silence by
  the longer of the wall-clock and the monotonic reading, as the client
  judges the server; a server clock set back no longer keeps a dead client.

## 0.9.8

### Fixed

- A connection that went silent without closing kept a `use_keep_ip` client
  offline for good. When nothing has arrived for four retry periods the
  checker reports `Disconnected` and dials again — but with `use_keep_ip` it
  left the silent connection standing, and that connection goes on holding
  the single-flight flag every reconnect path checks first. So the dial was
  turned away, on that look and on every look after it. Without
  `use_keep_ip` the connection was taken down as a side effect of forgetting
  the address, which is why only clients that keep their address were stuck.

  A socket goes silent without closing when a tablet's screen turns off and
  takes the network with it, and when a server drops a client from its list
  but keeps the socket: writes go through, nothing comes back, nothing
  errors. A server that is killed closes the socket, and that case already
  worked.

  The checker now takes the connection down either way, and `use_keep_ip`
  decides only whether the address goes with it. This is for clients that
  ping and are answered (`use_ping`, built with `bebop`): for them a link
  that returns no pong is dead. A client that does not ping hears nothing on
  a healthy link with nothing to say, so with `use_keep_ip` its silence is
  reported as it was before and the link is left alone.

  A client that forgets its address (`use_keep_ip` off) is taken down on
  silence as it always was, whether it pings or not. What changes is a build
  without `bebop`: there the connection used to stay up holding the flag,
  with nothing registered to send through, and now it really ends. On an
  idle link a client that forgets its address and does not ping is
  therefore reconnected about every five retry periods, as it already was
  with `bebop`.

- Dropping a connection now ends it, whatever state it is in. A connection
  was ended by queueing a "disconnect" message for its own writer to send
  and then recognise. That fails twice over. A writer parked on a socket
  that has stopped taking writes — the peer stopped acknowledging while the
  application went on sending — never gets to its queue. And a build without
  `bebop` has no such message: it queued an empty frame, the writer sent it
  and carried on, and the connection kept holding the single-flight flag
  with nothing registered to send through.

  Each connection now has a stop signal. Its writer watches it while idle
  and while a write is in flight, and `remove_ip`, `drop_connection`,
  `AtomicClient::disconnect` and a newer connection replacing an older one
  all end a connection through it.

  On a socket that still works, the writer first sends what `send` had
  already accepted and then tells the server goodbye, all within 250 ms. A
  write that is merely in flight when the signal arrives gets another
  250 ms before the connection is called parked and given up, and a parked
  one is closed without waiting to flush. What is queued behind a parked
  write, or does not fit in the 250 ms — a message of several hundred
  kilobytes on a slow link — goes with the connection. 0.9.7 queued the
  goodbye behind everything and so wrote the lot, on a socket that worked.
  A caller that runs its own writer on the receiver it passed to `add` now
  sees the channel close, where it used to be sent a Disconnect message.

  A `send` that is waiting for room in the queue when the connection is
  dropped on purpose — `remove_ip`, `AtomicClient::disconnect` — no longer
  brings the connection back. It used to fail when the queue closed and
  redial the address it had captured.

- A connection that ends now closes its socket. The task reading from the
  server used to be left running when this end gave up first; on a socket
  the other side never closed it stayed forever, kept the socket open, and
  would have stamped the client "heard from the server" if the old socket
  ever delivered anything. It is stopped with the connection. On a socket
  that still works and has been heard from recently it is first given a
  moment to end by itself — a server that has read the goodbye hangs up —
  because closing with unread data makes the OS reset the connection, and a
  reset can take the last messages written with it. A connection that has
  gone quiet is closed at once. What it had read and not yet handed on goes
  with the connection.

- A connection made a moment ago is no longer judged by its predecessor's
  last message. The checker measured silence from the last message on any
  connection, so between a reconnect and its first pong the new connection
  looked exactly as dead as the old one, and a look that fell in that gap
  took it down again. Silence is now counted from the later of the last
  message and the moment the connection was registered. A connection that
  has not been heard from yet does not reset the backoff between redials
  either, so a server that accepts connections and answers none is still
  dialed less and less often. A registration time later than the clock —
  the clock was set back — is not believed.

- The dial that follows a dead connection no longer loses a whole backoff
  step. It was started in the same breath as the teardown and usually
  arrived while the old connection still held the flag. It now waits up to
  two seconds for the flag.

- A `send` that was waiting on a connection which has since been dropped or
  replaced no longer reports `Disconnected` when it gives up. It reported
  the failure of a connection that had already been dealt with, and that
  put `Disconnected` after the replacement's `Connected`: an application
  that mirrors the status showed Offline over a working connection.

- The checker's ping has a deadline of one retry period. It went through the
  connection's queue with none, so on a connection whose queue was full
  behind a parked writer the checker waited there and never got as far as
  calling the connection dead.

- A dial that fails no longer costs a `use_keep_ip` client its address. The
  connect path cleared the stored address on any failed attempt, kept or
  not. One attempt while the server was restarting, or out of reach for a
  few seconds, and the client had nowhere left to dial: it came back only if
  discovery was on and a subnet scan found the server — most of a minute —
  or the application handed it the address again. A client that keeps its
  address now keeps it through failed dials, and whatever asks for a
  connection next dials it again: the application, or the checker once the
  client has heard from a server in this run. Without `use_keep_ip` nothing
  changes: the address goes with the first failure.

  Because the address is no longer forgotten, the same failure can repeat
  for as long as the server is away. It is logged the first time and then
  once in twenty.

### Changed

- With `use_keep_ip` and `use_scan_discovery` both on, discovery now runs
  *beside* the dials of the kept address instead of only after the address
  has been thrown away. This is for internal clients built with `native-db`
  and `bebop`; a client that dials a URL has no stored address to give up
  on. Once three dials in a row have failed, the next request for a
  connection also starts a scan of the local subnet in the background, and
  dials the kept address as usual. A server that comes back where it was is
  reconnected by the dial; one that moved is found by the scan, and what the
  scan finds replaces the kept address.

  The scan reports no status of its own. It stops as soon as a connection
  is made some other way. If it finds a server while a dial is in flight it
  waits `connect_timeout_seconds` and one second more for the single-flight
  flag; if the flag is still held then, the dial got through and the found
  connection is dropped. After a scan that finds nothing, three more dials
  have to fail before the next one starts.

  `is_scanning` is set for the length of such a scan, as for any other. An
  application that stops asking for connections while it is set will want to
  go on asking when the client has a kept address, or the dials beside the
  scan are only the library's own.

### Added

- `ServerSender::drop_connection`: end the current connection and keep the
  server address. What `remove_ip` does to the connection, without what it
  does to the address. (Both only touch the address held in memory; the
  stored copy is cleared by `remove_ip_if_valid_server_ip` alone.)
- `ServerSender::connected_at`: when the current connection was registered.
- `ServerSender::failed_dials`: how many dials of a kept address have failed
  in a row. Zero again when a connection is made, when the address is
  forgotten, and when a scan beside the dials starts or ends empty-handed.

## 0.9.7

### Fixed

- The connection-cap refusal logged the wrong number. It added the free
  permits (zero, by definition, at a refusal) to the count of peers that had
  finished registering, so it printed whatever happened to be connected at
  that instant — "already at the 72 connection cap" on a server configured
  for 128, and a different number each time. It now prints the configured
  cap, which is the thing an operator can go and change.

## 0.9.6

### Added

- `ServerOptions::max_connections` (default 512), a cap on connections
  accepted at once. A permit is held for the life of a connection and
  released when it ends, so this bounds sockets in flight rather than
  connections per second. Over the cap the TCP connection is closed rather
  than queued — a client that cannot be served should find out now and retry
  instead of holding a socket open waiting for a slot. Also on
  `ServerOptionsBuilder`.

  There was no bound before: every accepted socket spawned a task, so
  anything opening connections faster than they closed grew until the
  process died. The TLS path takes its permit before the handshake, since
  the handshake is itself work an unbounded number of callers could pile on.

## 0.9.5

### Fixed

- `ClientSenders::send` no longer waits without a deadline. The channel is
  bounded and is drained by the connection's writer task; when that task is
  blocked writing to a socket, `send().await` waited for capacity that could
  not arrive until the OS gave up on the socket — minutes, for a device that
  left WiFi mid-session. `send_all` joins across every peer, so one
  unreachable client held up every broadcast for that whole time. Sends now
  give up after 2s and the peer is dropped.

- The exponential backoff around that send is gone. A bounded `Sender` only
  errors once its receiver has been dropped, and a dropped receiver does not
  come back, so all five retries were guaranteed to fail — they added about
  two seconds before returning the answer the first attempt already had. It
  also never covered the case above, where the send does not error at all.

## 0.9.4

* **Breaking:** Remove `ClientOptions::use_tls` (and `ClientOptionsBuilder::use_tls`) — it was written by the builder and read by nobody. TLS on the outer client has always been chosen by the URL scheme, with a scheme-less address defaulting from the `rustls` feature; setting the field did nothing.
  * The scheme rule is now a named function, `outer_ws_url`, shared by both `get_outer_websocket` implementations instead of being written out twice. `ClientOptions::url` documents it directly: give `ws://`/`wss://` explicitly, since a scheme-less address silently speaks a different protocol depending on how the binary was compiled, and the resulting failure — a TLS handshake error against a plaintext server — reads like a certificate problem rather than an address problem.
  * No consumer sets `use_tls` today, so nothing outside this repo has to change; the field simply no longer exists.
  * Added `outer_ws_url_tests`, pinning both directions: an explicit scheme survives untouched, a scheme-less one picks up the build's default.

## 0.9.3

* Fix one reconnecting client freezing broadcasts to every other connected client.
  * `ClientSenders::add` replaces an existing connection and, on the way, notifies the old one to disconnect — but it did that *while holding the DashMap shard's write lock*, awaiting a bounded channel that only the old connection's writer task drains. When a client walks out of WiFi range and comes back (an ordinary event, not an edge case, in a room full of tablets), the old socket is half-open: its writer is blocked in `send`, the 8-slot channel fills, and the notice never lands until the OS gives up on the socket minutes later — with the shard lock held for all of it. Since dashmap's `RwLock` is task-fair, every later reader queued behind that held write lock too, including `peers()`, where every broadcast starts. One returning client stopped broadcasts to all of them.
  * `add` now clones the previous sender, drops the shard guard, and only then sends the disconnect notice under a 200ms timeout — best-effort, since a wedged peer's notice is worthless anyway and the replacement landing is what actually matters.
  * Also stopped `connections_active` from climbing on every reconnect of the same peer and never coming back down; a replacement is not a new connection.
  * Added `tests/reconnect_blocking_test.rs`, which reproduces all three failure modes with no server or client needed — a channel nobody drains is a faithful stand-in for a socket nobody reads.

## 0.9.2

* Normalize the stored server address before dialing, so a bare IP connects instead of silently failing.
  * The address persisted in `ServerConnectInfo` was passed straight to `tokio_tungstenite::connect_async`, which requires a full WebSocket URL. `ScanManager` produces `ws://ip:port`, but an address that reached storage any other way — a bare IP typed into a host app's "server IP" field, or a record written by an older build — has no scheme.
  * `connect_async` then failed at URL-parse time, *before* any `send_status` call, so the client emitted neither `Connected` nor `Disconnected`. It never connected and never reported why: observed in the field as a terminal that opened no socket at all, with every host-side retry loop waiting forever on a status transition that could not arrive.
  * `get_internal_connect`'s direct-connect branch now runs the address through `to_ws_url`, which adds the scheme (and the port, when the address doesn't carry one) and leaves an address that is already a URL untouched — including a non-`ws` scheme. Since `handle_websocket` persists whatever address it connected with, the normalized form also replaces the malformed record on the first success.
  * Consumers no longer need to format the address themselves before storing it.
  * Added `test_to_ws_url`.

## 0.9.1

* Fix the persisted port being set to the *host* after any successful connection, which permanently broke reconnection for that install.
  * `RwServerSender::add` derived the port with `server_ip.split(':').nth(1)`. Since `server_ip` is a full WebSocket URL (`ws://10.0.0.5:16250`) — the form `ScanManager` produces and callers store — the scheme's own colon matched first, so index 1 was `"//10.0.0.5"`, not `16250`.
  * That value was then persisted alongside the IP, and `clear_server_ip` (whose whole purpose is to keep the port while blanking the IP) faithfully preserved it on every disconnect. From then on the stored connect info read `("", "//10.0.0.5")`, and each reconnect handed that garbage to `ScanManager::new` and the port-reserve path. Observed in the field as a client that connects once, drops, and then retries forever without ever reconnecting.
  * The port is now taken from the last colon-separated segment and only accepted if it is a run of ASCII digits, so an address without a port stores `""` rather than the host. IPv6 literals (`ws://[::1]:16250`) and a trailing `/` are handled.
  * `clear_server_ip` additionally drops a non-numeric port instead of preserving it, so records already written by an older build heal on the next disconnect rather than staying poisoned.
  * Added `test_parse_port` and `test_clear_server_ip_drops_non_numeric_port`.

## 0.9.0

* **Breaking:** Decouple the client-side connection APIs from the host app's `db` handle.
  * Added `connection_store::ConnectionStore` — a small trait covering exactly the state the library manages internally (client ID, last-known server connect info), plus `NativeDbConnectionStore`, a bundled implementation that is a behavior-preserving wrapper around the existing `Settings`-table logic (same keys, same serialization, same `spawn_blocking` handling of the redb fsync).
  * `ServerSender::new`, `AtomicWebsocket::get_internal_client*`/`get_outer_client*`, `AtomicClient::internal_initialize`/`outer_initialize`/`regist_id`/`scan_and_connect`/`get_outer_connect`/`get_internal_connect`, and the free functions `get_outer_connect`/`get_internal_connect` now take `connection_store: Arc<dyn ConnectionStore>` instead of `db: DB`. Migration is a one-line wrap at each call site: `Arc::new(NativeDbConnectionStore::new(db.clone()))` built once, in place of `db.clone()`.
  * Unaffected: `DB`, `Settings`, `save_key`, and the generic `get_setting_by_key`/`set_setting`/`remove_setting`/`get_id` helpers are unchanged — apps using them directly for their own settings, or reading/writing the `Settings` table directly for `atomic_websocket`'s own keys, need no changes. The server side (`AtomicServer`/`ClientSenders`) was already decoupled from `db` in 0.7.0 and is unaffected.

## 0.8.4

* Fix a race that allowed two reconnect attempts to both dial the server concurrently, leading to reconnect instability under flaky/loaded connections.
  * `is_try_connect` was only set to `true` inside `handle_websocket`, i.e. *after* the handshake (`connect_async`) already succeeded. Between "a reconnect is decided" and "handshake succeeds" there was an unguarded window (bounded by `connect_timeout_seconds`, longer under real network stress) during which independent reconnect triggers — the periodic ping-loop checker, an app-level `get_internal_connect`/`get_outer_connect` call, or `ServerSender::send`'s exhausted-retry reconnect — could each start their own connection attempt. If two landed, `ServerSender::add()` drops the *previous* sender before installing the new one, so an earlier, healthy connection could get killed out from under itself the moment a duplicate arrived.
  * `is_try_connect` is now claimed atomically via a `TryConnectGuard` right before the handshake starts (`get_internal_websocket`/`get_outer_websocket`, and the scan-found handoff paths), not after it succeeds. A concurrent trigger during the same window now correctly no-ops instead of also dialing. The guard resets itself on drop, so a failed/timed-out handshake can no longer leave `is_try_connect` stuck `true` either (previously only the success path reset it).
  * Added a regression test (`test_concurrent_connect_triggers_dial_only_once`) that races two concurrent reconnect triggers and asserts only one connection reaches the server; reliably reproduces the duplicate-dial bug against the pre-fix code.

## 0.8.3

* Fix unbounded socket accumulation in the scan-discovery path (could exhaust the machine's ephemeral ports).
  * Single-flight the scan via a new `is_scanning` guard. Previously `is_try_connect` only became true once a connection was established, so while `ScanManager` was still searching (forever, when no server exists) every repeated `get_internal_connect` / `scan_and_connect` call started another concurrent scan — each holding a subnet's worth of in-flight sockets.
  * Bound the discovery scan with `run_with_timeout(scan_timeout_seconds)` instead of the unbounded `run()`; on timeout the client reports `Disconnected` so the caller's retry loop stays in charge.
  * Added a single-flight regression test.

## 0.8.2

* Expose `scan_manager` (`ScanManager`) publicly again. 0.8.1 made `helpers` private, which removed `atomic_websocket::scan_manager::ScanManager` from the public API; Android clients pre-scan the LAN for the POS with it before the first connect. Additive re-export, no behavior change.

## 0.8.1

* Improve low-spec stability and make server discovery explicit for fixed-IP deployments.
  * Run every native-db (redb) transaction on a blocking thread via `spawn_blocking`, so the synchronous `commit()` `fsync` never stalls a Tokio worker thread. On slow disks this previously drifted the ping loop timing and caused false disconnections / reconnection storms. The public `DB` type is unchanged (uses `blocking_lock()` internally).
  * Connect directly to a known fixed server IP without depending on `get_ip_address()` / internet reachability — works on isolated LANs with no route to the outside. The local IP is now resolved only on the scan path.
  * **Behavior change:** local subnet auto-scan is now opt-in. Added `ClientOptions::use_scan_discovery` (default `false`); when the server IP is unknown the client reports `Disconnected` and the app keeps running instead of scanning. Trigger discovery explicitly with the new `AtomicClient::scan_and_connect()` ("search" button), bounded by `ClientOptions::scan_timeout_seconds` (default 60). Added `ScanManager::run_with_timeout()` and matching builder setters.
  * `test_client` now takes `<server_ip> <port> <client_id>` arguments and persists them to the database, and starts even when the server is unreachable. Fixed pre-existing compile errors in `test_client`/`test_server`.
  * Replaced the unmaintained/archived `rustls-pemfile` (RUSTSEC-2025-0134) with the PEM parsing in `rustls-pki-types` (re-exported by `rustls`), removing a dependency. Only affects the `rustls` feature.
  * Added stress/regression tests for runtime blocking (`tests/stress_blocking_test.rs`) and fixed-IP behavior (`tests/fixed_ip_test.rs`).

## 0.8.0

* Improve performance, connection stability and WebSocket upgrade with safer APIs.

## 0.7.5

* Remove send timeout to prevent false disconnection under backpressure

## 0.7.4

* Add spillover for dropped client send message

## 0.7.3

* Add spillover for dropped send message

## 0.7.2

* Add metrics, middleware, TLS support and refactor interior mutability.

## 0.7.1

* Improve connection stabiliation.

## 0.7.0

* Refactor code base, remove db dependency.

## 0.6.33

* Add send_all_in_list in clientSenders.

## 0.6.32

* Combine serverOption in clientSenders.

## 0.6.31

* Fixed proxy_ping with server.

## 0.6.30

* Fixed use_ping with false when connected first.

## 0.6.29

* Fixed use_ping for loop_checker.

## 0.6.28

* Fixed loop_checker for external connection.

## 0.6.27

* Add README.md and doc in files.

## 0.6.26

* Fixed get_ip_address for local wifi.

## 0.6.25

* Fixed remove release connect when connect failed.

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