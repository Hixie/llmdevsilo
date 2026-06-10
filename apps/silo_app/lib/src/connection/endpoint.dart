/// A known harness endpoint as persisted in the registry.
library;

/// One configured harness. Values associated with the endpoint live in
/// stores under keys derived from [id]: the local token and Ed25519
/// private key in the secret store, the key id and pinned certificate
/// fingerprint in the preferences store.
class HarnessEndpoint {
  const HarnessEndpoint({
    required this.id,
    required this.name,
    required this.url,
    this.harnessId,
  });

  /// Stable identifier, generated when the endpoint is added.
  final String id;

  /// Display name.
  final String name;

  /// WebSocket URL, e.g. `wss://127.0.0.1:7777`.
  final String url;

  /// The harness's own identifier, from its run file or Hello message.
  /// Null until learned. Endpoints with the same harness id are the same
  /// harness, so the registry keeps only one entry per harness id.
  final String? harnessId;

  HarnessEndpoint copyWith({String? name, String? url, String? harnessId}) =>
      HarnessEndpoint(
        id: id,
        name: name ?? this.name,
        url: url ?? this.url,
        harnessId: harnessId ?? this.harnessId,
      );

  factory HarnessEndpoint.fromJson(Map<String, dynamic> json) =>
      HarnessEndpoint(
        id: json['id'] as String,
        name: json['name'] as String,
        url: json['url'] as String,
        harnessId: json['harness_id'] as String?,
      );

  Map<String, dynamic> toJson() => {
        'id': id,
        'name': name,
        'url': url,
        if (harnessId != null) 'harness_id': harnessId,
      };

  /// Secret-store key for the local auth token.
  String get tokenKey => 'silo/$id/token';

  /// Secret-store key for the Ed25519 private key seed (base64).
  String get keySeedKey => 'silo/$id/key_seed';

  /// Preferences key for the key id assigned at pairing.
  String get keyIdKey => 'silo/$id/key_id';

  /// Preferences key for the pinned certificate fingerprint (hex SHA-256).
  String get fingerprintKey => 'silo/$id/fingerprint';
}

/// The last non-empty path component of [workspace], with both slash
/// styles treated as separators. Null when [workspace] is null or has no
/// non-empty component.
String? workspaceFolderName(String? workspace) {
  if (workspace == null) {
    return null;
  }
  for (final part in workspace.split(RegExp(r'[/\\]')).reversed) {
    final trimmed = part.trim();
    if (trimmed.isNotEmpty) {
      return trimmed;
    }
  }
  return null;
}

/// Display name for a harness. Preference order: the user-given endpoint
/// name, the workspace folder name, the host of [url], and finally [url].
/// A name equal to [harnessId] counts as absent, so the raw harness id
/// never surfaces as a display name.
String harnessDisplayName({
  String? endpointName,
  String? workspace,
  String? harnessId,
  required String url,
}) {
  final name = endpointName?.trim() ?? '';
  if (name.isNotEmpty && name != harnessId) {
    return name;
  }
  final folder = workspaceFolderName(workspace);
  if (folder != null) {
    return folder;
  }
  final host = Uri.tryParse(url)?.host;
  if (host != null && host.isNotEmpty) {
    return host;
  }
  return url;
}
