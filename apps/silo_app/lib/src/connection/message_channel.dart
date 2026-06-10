/// Transport abstraction for the harness WebSocket connection, so the
/// connection logic can be driven by a mock channel in tests.
library;

import 'package:web_socket_channel/web_socket_channel.dart';

/// A bidirectional text-message channel.
abstract class MessageChannel {
  /// Incoming WebSocket text frames. Errors and `done` signal disconnection.
  Stream<dynamic> get stream;

  /// Sends one text frame.
  void add(String data);

  Future<void> close();
}

/// Opens a channel to [uri]. [fingerprintSha256] is the expected SHA-256
/// fingerprint (lowercase hex) of the server's TLS certificate, used for
/// pinning on platforms that allow it; null skips pinning.
typedef ChannelFactory = Future<MessageChannel> Function(
  Uri uri,
  String? fingerprintSha256,
);

/// [MessageChannel] backed by a real WebSocket.
class WebSocketMessageChannel implements MessageChannel {
  WebSocketMessageChannel(this._channel);

  final WebSocketChannel _channel;

  @override
  Stream<dynamic> get stream => _channel.stream;

  @override
  void add(String data) => _channel.sink.add(data);

  @override
  Future<void> close() async {
    await _channel.sink.close();
  }
}
