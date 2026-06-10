/// WebSocket channel factory for the web. Browsers do not expose TLS
/// certificate details to page code, so certificate pinning is not possible
/// here: the fingerprint is ignored and the browser's normal certificate
/// validation applies. The harness therefore needs a certificate the browser
/// trusts (for example one issued by a real certificate authority) for web
/// clients to connect.
library;

import 'package:web_socket_channel/web_socket_channel.dart';

import 'message_channel.dart';

Future<MessageChannel> platformConnect(
  Uri uri,
  String? fingerprintSha256,
) async {
  final channel = WebSocketChannel.connect(uri);
  await channel.ready;
  return WebSocketMessageChannel(channel);
}
