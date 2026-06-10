/// WebSocket channel factory for platforms with `dart:io` (desktop and
/// mobile): connects with TLS certificate pinning.
library;

import 'dart:io';

import 'package:crypto/crypto.dart' as crypto;
import 'package:web_socket_channel/io.dart';

import 'message_channel.dart';

/// Connects to [uri] over TLS, accepting exactly the certificate whose
/// SHA-256 fingerprint (hex over the DER encoding) matches
/// [fingerprintSha256]. The harness uses a self-signed certificate, so
/// normal chain validation always fails and the pin is the sole trust
/// decision. With a null fingerprint the connection only succeeds if the
/// certificate validates against the system roots.
Future<MessageChannel> platformConnect(
  Uri uri,
  String? fingerprintSha256,
) async {
  final client = HttpClient();
  final expected = fingerprintSha256?.toLowerCase();
  client.badCertificateCallback = (X509Certificate cert, String host, int port) {
    if (expected == null) {
      return false;
    }
    final actual = crypto.sha256.convert(cert.der).toString();
    return actual == expected;
  };
  final channel = IOWebSocketChannel.connect(
    uri,
    customClient: client,
    connectTimeout: const Duration(seconds: 15),
  );
  await channel.ready;
  return WebSocketMessageChannel(channel);
}
