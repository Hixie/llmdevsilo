/// Persisted list of known harnesses and the live connections to them.
library;

import 'dart:async';
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
const _migratedKey = 'silo/storage-migrated';

/// Holds the configured harness endpoints and one [HarnessConnection] per
/// endpoint. Multiple connections can be open simultaneously.
///
/// Persistence is split by sensitivity: the endpoint list, the silo path,
/// the launch form, pinned fingerprints, and key ids go to [_settings]
/// (the preferences document); local tokens and pairing key seeds go to
/// [_secrets] (the consolidated keystore document). [load] migrates
/// values left in the legacy one-keystore-item-per-key layout when a
/// legacy store is provided.
class HarnessRegistry extends ChangeNotifier {
  HarnessRegistry({
    required this._secrets,
    required this._settings,
    this._legacySecrets,
    this._channelFactory,
    this.clientName = 'silo_app',
    this._backoff,
  });

  final SecretStore _secrets;
  final SecretStore _settings;
  final SecretStore? _legacySecrets;
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

  /// Reads a key from [store], treating storage failures as absent values
  /// so a broken store degrades to defaults instead of preventing startup.
  Future<String?> _tryRead(SecretStore store, String key) async {
    try {
      return await store.read(key);
    } catch (error) {
      debugPrint('store read of $key failed: $error');
      return null;
    }
  }

  Future<void> _tryWrite(SecretStore store, String key, String value) async {
    try {
      await store.write(key, value);
    } catch (error) {
      debugPrint('store write of $key failed: $error');
    }
  }

  Future<void> _tryDelete(SecretStore store, String key) async {
    try {
      await store.delete(key);
    } catch (error) {
      debugPrint('store delete of $key failed: $error');
    }
  }

  Future<void> load() async {
    if (await _tryRead(_settings, _migratedKey) == null) {
      await _migrateLegacyStorage();
    }
    final raw = await _tryRead(_settings, _registryKey);
    _endpoints.clear();
    if (raw != null) {
      try {
        final list = jsonDecode(raw) as List<dynamic>;
        _endpoints.addAll(list
            .map((e) => HarnessEndpoint.fromJson(e as Map<String, dynamic>)));
      } catch (error) {
        debugPrint('unreadable registry document dropped: $error');
      }
    }
    await _dedupeEndpoints();
    _siloPath = await _tryRead(_settings, _siloPathKey);
    final lastLaunch = await _tryRead(_settings, _lastLaunchKey);
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

  /// Consolidates values stored one keystore item per key into the
  /// single-document stores. Best-effort throughout: a denied keystore
  /// read skips that value and the consolidation still completes, so the
  /// app never gets stuck re-prompting.
  Future<void> _migrateLegacyStorage() async {
    final legacy = _legacySecrets;
    if (legacy != null &&
        await _tryRead(_settings, _registryKey) == null) {
      final raw = await _tryRead(legacy, _registryKey);
      if (raw != null) {
        List<HarnessEndpoint> endpoints = const [];
        try {
          endpoints = (jsonDecode(raw) as List<dynamic>)
              .map((e) => HarnessEndpoint.fromJson(e as Map<String, dynamic>))
              .toList();
        } catch (error) {
          debugPrint('unreadable legacy registry dropped: $error');
        }
        for (final endpoint in endpoints) {
          // Non-secrets move to the preferences document.
          for (final key in [endpoint.fingerprintKey, endpoint.keyIdKey]) {
            final value = await _tryRead(legacy, key);
            if (value != null) {
              await _tryWrite(_settings, key, value);
            }
            await _tryDelete(legacy, key);
          }
          // Real secrets move to the consolidated keystore document.
          for (final key in [endpoint.tokenKey, endpoint.keySeedKey]) {
            final value = await _tryRead(legacy, key);
            if (value != null) {
              await _tryWrite(_secrets, key, value);
            }
            await _tryDelete(legacy, key);
          }
        }
        await _tryWrite(_settings, _registryKey, raw);
        await _tryDelete(legacy, _registryKey);
      }
      for (final key in [_siloPathKey, _lastLaunchKey]) {
        final value = await _tryRead(legacy, key);
        if (value != null) {
          await _tryWrite(_settings, key, value);
        }
        await _tryDelete(legacy, key);
      }
    }
    await _tryWrite(_settings, _migratedKey, '1');
  }

  /// Collapses entries that point at the same harness (same harness id,
  /// or same URL). The most recently added entry wins, unless only the
  /// earlier entry has stored credentials, in which case the credentialed
  /// entry wins. The values of removed entries are deleted from both
  /// stores.
  Future<void> _dedupeEndpoints() async {
    final kept = <HarnessEndpoint>[];
    final removed = <HarnessEndpoint>[];
    for (final endpoint in _endpoints) {
      final index = kept.indexWhere((e) => _sameHarness(e, endpoint));
      if (index < 0) {
        kept.add(endpoint);
        continue;
      }
      final earlier = kept[index];
      var winner = endpoint;
      var loser = earlier;
      if (await _hasCredentials(earlier) && !await _hasCredentials(endpoint)) {
        winner = earlier;
        loser = endpoint;
      }
      if (winner.harnessId == null && loser.harnessId != null) {
        winner = winner.copyWith(harnessId: loser.harnessId);
      }
      kept[index] = winner;
      removed.add(loser);
    }
    if (removed.isEmpty) {
      return;
    }
    _endpoints
      ..clear()
      ..addAll(kept);
    for (final endpoint in removed) {
      await _dropEndpointValues(endpoint);
      _retireConnection(_connections.remove(endpoint.id));
    }
    await _persist();
  }

  /// True when [endpoint] has a stored local token or pairing key seed.
  Future<bool> _hasCredentials(HarnessEndpoint endpoint) async =>
      await _tryRead(_secrets, endpoint.tokenKey) != null ||
      await _tryRead(_secrets, endpoint.keySeedKey) != null;

  /// Connections removed from the registry while still open; each is
  /// disposed once it reports disconnected or failed.
  final Set<HarnessConnection> _retiring = {};

  static bool _isIdle(ConnectionStatus status) =>
      status == ConnectionStatus.disconnected ||
      status == ConnectionStatus.failed;

  /// Disposes [connection] when it is idle. An open connection is kept
  /// alive and disposed once it reaches disconnected or failed, so a
  /// transcript in use never has its connection torn down underneath it.
  void _retireConnection(HarnessConnection? connection) {
    if (connection == null) {
      return;
    }
    if (_isIdle(connection.status)) {
      connection.dispose();
      return;
    }
    _retiring.add(connection);
    late final void Function() listener;
    listener = () {
      if (!_isIdle(connection.status)) {
        return;
      }
      connection.removeListener(listener);
      if (_retiring.remove(connection)) {
        // The status change arrives via notifyListeners, and disposing a
        // ChangeNotifier during its own dispatch is not allowed.
        scheduleMicrotask(connection.dispose);
      }
    };
    connection.addListener(listener);
  }

  static bool _sameHarness(HarnessEndpoint a, HarnessEndpoint b) =>
      (a.harnessId != null && a.harnessId == b.harnessId) || a.url == b.url;

  /// Saves the `silo` binary path; an empty value clears it.
  Future<void> setSiloPath(String path) async {
    final trimmed = path.trim();
    _siloPath = trimmed.isEmpty ? null : trimmed;
    // Preference persistence is best-effort: a storage failure keeps the
    // value for this session and logs instead of crashing.
    if (_siloPath == null) {
      await _tryDelete(_settings, _siloPathKey);
    } else {
      await _tryWrite(_settings, _siloPathKey, _siloPath!);
    }
    notifyListeners();
  }

  /// Saves the start-local-harness form state for the next opening of the
  /// dialog. Best-effort like [setSiloPath].
  Future<void> setLastLaunchForm(LocalHarnessFormState form) async {
    _lastLaunchForm = form;
    await _tryWrite(_settings, _lastLaunchKey, jsonEncode(form.toJson()));
    notifyListeners();
  }

  Future<void> _persist() async {
    await _tryWrite(
      _settings,
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
    return _connections.putIfAbsent(endpoint.id, () {
      final connection = HarnessConnection(
        endpoint: endpoint,
        secrets: _secrets,
        settings: _settings,
        channelFactory: _channelFactory,
        clientName: clientName,
        backoff: _backoff,
      );
      // The Hello handshake reveals the harness id; record it so later
      // adds of the same harness under another URL update this entry.
      connection.addListener(() => _noteHarnessId(endpoint.id, connection));
      return connection;
    });
  }

  void _noteHarnessId(String endpointId, HarnessConnection connection) {
    final harnessId = connection.harnessId;
    if (harnessId == null) {
      return;
    }
    final index = _endpoints.indexWhere((e) => e.id == endpointId);
    if (index < 0 || _endpoints[index].harnessId == harnessId) {
      return;
    }
    _endpoints[index] = _endpoints[index].copyWith(harnessId: harnessId);
    unawaited(_dedupeEndpoints().then((_) => _persist()));
    notifyListeners();
  }

  /// Returns the existing endpoint for [harnessId] or [url], updated in
  /// place, or appends a new one. The endpoint id (and with it the store
  /// keys) is preserved on update. A connection whose URL changed is
  /// dropped so the next use reconnects to the new address.
  HarnessEndpoint _upsert({
    required String name,
    required String url,
    String? harnessId,
  }) {
    final index = _endpoints.indexWhere((e) =>
        (harnessId != null && e.harnessId == harnessId) || e.url == url);
    if (index < 0) {
      final endpoint = HarnessEndpoint(
        id: _newId(),
        name: name,
        url: url,
        harnessId: harnessId,
      );
      _endpoints.add(endpoint);
      return endpoint;
    }
    final updated = _endpoints[index]
        .copyWith(name: name, url: url, harnessId: harnessId);
    _endpoints[index] = updated;
    final connection = _connections[updated.id];
    if (connection != null && connection.endpoint.url != url) {
      _connections.remove(updated.id);
      _retireConnection(connection);
    }
    return updated;
  }

  /// Adds a remote harness reached by pairing code, or updates the entry
  /// already pointing at [url]. The certificate fingerprint comes from
  /// the user (shown next to the pairing code on the harness side); it is
  /// pinned from the first connection.
  Future<HarnessConnection> addPaired({
    required String name,
    required String url,
    required String pairingCode,
    String? fingerprintSha256,
  }) async {
    final endpoint = _upsert(name: name, url: url);
    if (fingerprintSha256 != null && fingerprintSha256.isNotEmpty) {
      await _tryWrite(
        _settings,
        endpoint.fingerprintKey,
        fingerprintSha256.toLowerCase().replaceAll(':', ''),
      );
    }
    await _persist();
    final connection = connectionFor(endpoint);
    connection.pendingPairingCode = pairingCode;
    notifyListeners();
    return connection;
  }

  /// Adds a local harness with a token read from its run file, or updates
  /// the entry already pointing at the same harness id or URL.
  Future<HarnessConnection> addLocal({
    required String name,
    required String url,
    required String token,
    required String fingerprintSha256,
    String? harnessId,
  }) async {
    final endpoint = _upsert(name: name, url: url, harnessId: harnessId);
    await _secrets.write(endpoint.tokenKey, token);
    await _tryWrite(
      _settings,
      endpoint.fingerprintKey,
      fingerprintSha256.toLowerCase(),
    );
    await _persist();
    final connection = connectionFor(endpoint);
    notifyListeners();
    return connection;
  }

  Future<void> _dropEndpointValues(HarnessEndpoint endpoint) async {
    await _tryDelete(_secrets, endpoint.tokenKey);
    await _tryDelete(_secrets, endpoint.keySeedKey);
    await _tryDelete(_settings, endpoint.keyIdKey);
    await _tryDelete(_settings, endpoint.fingerprintKey);
  }

  /// Removes the endpoint, closes its connection, and deletes its stored
  /// values.
  Future<void> remove(String endpointId) async {
    final index = _endpoints.indexWhere((e) => e.id == endpointId);
    if (index < 0) {
      return;
    }
    final endpoint = _endpoints.removeAt(index);
    final connection = _connections.remove(endpointId);
    await connection?.disconnect();
    _retireConnection(connection);
    await _dropEndpointValues(endpoint);
    await _persist();
    notifyListeners();
  }

  @override
  void dispose() {
    for (final connection in _connections.values) {
      connection.dispose();
    }
    _connections.clear();
    for (final connection in _retiring) {
      connection.dispose();
    }
    _retiring.clear();
    super.dispose();
  }
}
