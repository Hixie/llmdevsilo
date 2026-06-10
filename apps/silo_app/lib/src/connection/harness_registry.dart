/// Persisted list of known harnesses and the live connections to them.
library;

import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';

import 'endpoint.dart';
import 'harness_connection.dart';
import 'message_channel.dart';
import 'secret_store.dart';

const _registryKey = 'silo/registry';

/// Holds the configured harness endpoints (persisted via the secret store)
/// and one [HarnessConnection] per endpoint. Multiple connections can be
/// open simultaneously.
class HarnessRegistry extends ChangeNotifier {
  HarnessRegistry({
    required this._secrets,
    this._channelFactory,
    this.clientName = 'silo_app',
    this._backoff,
  });

  final SecretStore _secrets;
  final ChannelFactory? _channelFactory;
  final BackoffPolicy? _backoff;
  final String clientName;

  final List<HarnessEndpoint> _endpoints = [];
  final Map<String, HarnessConnection> _connections = {};

  List<HarnessEndpoint> get endpoints => List.unmodifiable(_endpoints);

  bool _loaded = false;
  bool get loaded => _loaded;

  Future<void> load() async {
    final raw = await _secrets.read(_registryKey);
    _endpoints.clear();
    if (raw != null) {
      final list = jsonDecode(raw) as List<dynamic>;
      _endpoints.addAll(list
          .map((e) => HarnessEndpoint.fromJson(e as Map<String, dynamic>)));
    }
    _loaded = true;
    notifyListeners();
  }

  Future<void> _persist() async {
    await _secrets.write(
      _registryKey,
      jsonEncode(_endpoints.map((e) => e.toJson()).toList()),
    );
  }

  static String _newId() {
    final random = Random.secure();
    return List.generate(16, (_) => random.nextInt(16).toRadixString(16))
        .join();
  }

  /// The connection for [endpoint], created on first use.
  HarnessConnection connectionFor(HarnessEndpoint endpoint) {
    return _connections.putIfAbsent(
      endpoint.id,
      () => HarnessConnection(
        endpoint: endpoint,
        secrets: _secrets,
        channelFactory: _channelFactory,
        clientName: clientName,
        backoff: _backoff,
      ),
    );
  }

  /// Adds a remote harness reached by pairing code. The certificate
  /// fingerprint comes from the user (shown next to the pairing code on the
  /// harness side); it is pinned from the first connection.
  Future<HarnessConnection> addPaired({
    required String name,
    required String url,
    required String pairingCode,
    String? fingerprintSha256,
  }) async {
    final endpoint = HarnessEndpoint(id: _newId(), name: name, url: url);
    if (fingerprintSha256 != null && fingerprintSha256.isNotEmpty) {
      await _secrets.write(
        endpoint.fingerprintKey,
        fingerprintSha256.toLowerCase().replaceAll(':', ''),
      );
    }
    _endpoints.add(endpoint);
    await _persist();
    final connection = connectionFor(endpoint);
    connection.pendingPairingCode = pairingCode;
    notifyListeners();
    return connection;
  }

  /// Adds a local harness with a token read from its run file.
  Future<HarnessConnection> addLocal({
    required String name,
    required String url,
    required String token,
    required String fingerprintSha256,
  }) async {
    final endpoint = HarnessEndpoint(id: _newId(), name: name, url: url);
    await _secrets.write(endpoint.tokenKey, token);
    await _secrets.write(
      endpoint.fingerprintKey,
      fingerprintSha256.toLowerCase(),
    );
    _endpoints.add(endpoint);
    await _persist();
    final connection = connectionFor(endpoint);
    notifyListeners();
    return connection;
  }

  /// Removes the endpoint, closes its connection, and deletes its secrets.
  Future<void> remove(String endpointId) async {
    final index = _endpoints.indexWhere((e) => e.id == endpointId);
    if (index < 0) {
      return;
    }
    final endpoint = _endpoints.removeAt(index);
    final connection = _connections.remove(endpointId);
    await connection?.disconnect();
    connection?.dispose();
    await _secrets.delete(endpoint.tokenKey);
    await _secrets.delete(endpoint.keySeedKey);
    await _secrets.delete(endpoint.keyIdKey);
    await _secrets.delete(endpoint.fingerprintKey);
    await _persist();
    notifyListeners();
  }

  @override
  void dispose() {
    for (final connection in _connections.values) {
      connection.dispose();
    }
    _connections.clear();
    super.dispose();
  }
}
