/// Persisted list of known harnesses and the live connections to them.
library;

import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';

import 'endpoint.dart';
import 'harness_connection.dart';
import 'local_harness_options.dart';
import 'message_channel.dart';
import 'secret_store.dart';

const _registryKey = 'silo/registry';
const _siloPathKey = 'silo/silo-path';
const _lastLaunchKey = 'silo/last-launch-options';

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

  String? _siloPath;

  /// User-configured path of the `silo` binary, persisted alongside the
  /// endpoint list. Null when none has been saved.
  String? get siloPath => _siloPath;

  LocalHarnessFormState? _lastLaunchForm;

  /// Field values of the last start-local-harness form, persisted so the
  /// dialog can prefill them. Null when none has been saved.
  LocalHarnessFormState? get lastLaunchForm => _lastLaunchForm;

  /// Reads a key, treating storage failures as absent values so a broken
  /// keystore degrades to defaults instead of preventing startup.
  Future<String?> _tryRead(String key) async {
    try {
      return await _secrets.read(key);
    } catch (error) {
      debugPrint('secret store read of $key failed: $error');
      return null;
    }
  }

  Future<void> load() async {
    final raw = await _tryRead(_registryKey);
    _endpoints.clear();
    if (raw != null) {
      final list = jsonDecode(raw) as List<dynamic>;
      _endpoints.addAll(list
          .map((e) => HarnessEndpoint.fromJson(e as Map<String, dynamic>)));
    }
    _siloPath = await _tryRead(_siloPathKey);
    final lastLaunch = await _tryRead(_lastLaunchKey);
    _lastLaunchForm = null;
    if (lastLaunch != null) {
      try {
        _lastLaunchForm = LocalHarnessFormState.fromJson(
            jsonDecode(lastLaunch) as Map<String, dynamic>);
      } catch (_) {
        // An unreadable document is dropped; the dialog falls back to its
        // defaults.
      }
    }
    _loaded = true;
    notifyListeners();
  }

  /// Saves the `silo` binary path; an empty value clears it.
  Future<void> setSiloPath(String path) async {
    final trimmed = path.trim();
    _siloPath = trimmed.isEmpty ? null : trimmed;
    // Preference persistence is best-effort: a keystore failure keeps the
    // value for this session and logs instead of crashing.
    try {
      if (_siloPath == null) {
        await _secrets.delete(_siloPathKey);
      } else {
        await _secrets.write(_siloPathKey, _siloPath!);
      }
    } catch (error) {
      debugPrint('saving the silo path failed: $error');
    }
    notifyListeners();
  }

  /// Saves the start-local-harness form state for the next opening of the
  /// dialog. Best-effort like [setSiloPath].
  Future<void> setLastLaunchForm(LocalHarnessFormState form) async {
    _lastLaunchForm = form;
    try {
      await _secrets.write(_lastLaunchKey, jsonEncode(form.toJson()));
    } catch (error) {
      debugPrint('saving the launch form failed: $error');
    }
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
