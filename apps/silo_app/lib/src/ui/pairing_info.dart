/// Pure helpers behind the pairing sheet and the web connection guidance:
/// classifying the connected host, building candidate LAN URLs, deriving
/// the browser origin for certificate acceptance, and assembling the
/// copyable connection-details block.
library;

/// True when [host] is a loopback name or address: `localhost`, anything
/// in 127.0.0.0/8, or IPv6 `::1`.
bool isLoopbackHost(String host) {
  final lower = host.toLowerCase();
  return lower == 'localhost' ||
      lower == '::1' ||
      lower == '[::1]' ||
      lower.startsWith('127.');
}

/// True when [host] is the unspecified address (`0.0.0.0` or IPv6 `::`),
/// meaning the harness listens on every interface but the address as
/// written is not dialable from another device.
bool isUnspecifiedHost(String host) {
  return host == '0.0.0.0' || host == '::' || host == '[::]';
}

/// True when another device cannot use [host] as written, so the pairing
/// sheet should list candidate LAN URLs instead.
bool needsLanCandidates(String host) =>
    isLoopbackHost(host) || isUnspecifiedHost(host);

/// Builds one URL per address in [addresses], keeping the scheme and port
/// of [wsUrl].
List<String> lanCandidateUrls(String wsUrl, List<String> addresses) {
  final uri = Uri.parse(wsUrl);
  return [
    for (final address in addresses) uri.replace(host: address).toString(),
  ];
}

/// Derives the harness's `https://` (or `http://` for `ws://`) origin from
/// its WebSocket URL. Opening this address in a browser and accepting the
/// certificate warning lets the browser connect over `wss://` afterwards.
String httpsOriginFromWsUrl(String wsUrl) {
  final uri = Uri.parse(wsUrl);
  final scheme = uri.scheme == 'ws' ? 'http' : 'https';
  final host = uri.host.contains(':') ? '[${uri.host}]' : uri.host;
  final port = uri.hasPort ? ':${uri.port}' : '';
  return '$scheme://$host$port/';
}

/// The text block placed on the clipboard by "Copy connection details":
/// the URL (or URLs, when LAN candidates are listed), the certificate
/// fingerprint, and the pairing code.
String connectionDetailsBlock({
  required List<String> urls,
  String? fingerprint,
  String? code,
}) {
  final buffer = StringBuffer();
  if (urls.length == 1) {
    buffer.writeln('WebSocket URL: ${urls.single}');
  } else {
    buffer.writeln('WebSocket URLs:');
    for (final url in urls) {
      buffer.writeln('  $url');
    }
  }
  if (fingerprint != null) {
    buffer.writeln('Certificate fingerprint (SHA-256): $fingerprint');
  }
  if (code != null) {
    buffer.writeln('Pairing code: $code');
  }
  return buffer.toString();
}
