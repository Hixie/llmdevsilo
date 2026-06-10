/// A known harness endpoint as persisted in the registry.
library;

/// One configured harness. Secrets associated with the endpoint (local
/// token, Ed25519 private key, pinned certificate fingerprint) live in the
/// [SecretStore] under keys derived from [id], not in this record.
class HarnessEndpoint {
  const HarnessEndpoint({
    required this.id,
    required this.name,
    required this.url,
  });

  /// Stable identifier, generated when the endpoint is added.
  final String id;

  /// Display name.
  final String name;

  /// WebSocket URL, e.g. `wss://127.0.0.1:7777`.
  final String url;

  HarnessEndpoint copyWith({String? name, String? url}) => HarnessEndpoint(
        id: id,
        name: name ?? this.name,
        url: url ?? this.url,
      );

  factory HarnessEndpoint.fromJson(Map<String, dynamic> json) =>
      HarnessEndpoint(
        id: json['id'] as String,
        name: json['name'] as String,
        url: json['url'] as String,
      );

  Map<String, dynamic> toJson() => {'id': id, 'name': name, 'url': url};

  /// Secret-store key for the local auth token.
  String get tokenKey => 'silo/$id/token';

  /// Secret-store key for the Ed25519 private key seed (base64).
  String get keySeedKey => 'silo/$id/key_seed';

  /// Secret-store key for the key id assigned at pairing.
  String get keyIdKey => 'silo/$id/key_id';

  /// Secret-store key for the pinned certificate fingerprint (hex SHA-256).
  String get fingerprintKey => 'silo/$id/fingerprint';
}
