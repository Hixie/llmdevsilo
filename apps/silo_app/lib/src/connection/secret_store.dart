/// Storage for the app's persisted values.
///
/// Real secrets — pairing private keys and local auth tokens — live in a
/// single platform-keystore item holding one JSON document
/// ([JsonDocumentStore] over [SecureDocumentStore]). One item means the
/// macOS keychain asks for access at most once per run. Everything
/// non-secret (the endpoint list, pinned certificate fingerprints, key
/// ids, the silo path, the last launch form) lives in a plain JSON
/// preferences file; see `preferences.dart`.
library;

import 'dart:async';
import 'dart:convert';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// String key/value store. Implementations back it with a JSON document
/// ([JsonDocumentStore]) or with one keystore item per key
/// ([SecureSecretStore]). Tests use [MemorySecretStore].
abstract class SecretStore {
  Future<String?> read(String key);
  Future<void> write(String key, String value);
  Future<void> delete(String key);
}

/// One key per platform-keystore item: Keychain on macOS/iOS, the Android
/// keystore, and browser storage (encrypted with WebCrypto) on web.
///
/// The app reads this layout only to consolidate values stored
/// one-per-item into the single-document stores; every item is its own
/// keychain entry, so on macOS every read can raise its own permission
/// prompt.
class SecureSecretStore implements SecretStore {
  // The macOS data-protection keychain requires the keychain-access-groups
  // entitlement, which only builds under real development signing. The
  // legacy login keychain works with Flutter's default ad-hoc signing, so
  // a plain `flutter run -d macos` needs no Apple developer account.
  SecureSecretStore()
      : _storage = const FlutterSecureStorage(
          mOptions: MacOsOptions(useDataProtectionKeyChain: false),
        );

  final FlutterSecureStorage _storage;

  @override
  Future<String?> read(String key) => _storage.read(key: key);

  @override
  Future<void> write(String key, String value) =>
      _storage.write(key: key, value: value);

  @override
  Future<void> delete(String key) => _storage.delete(key: key);
}

/// In-memory store for tests.
class MemorySecretStore implements SecretStore {
  final Map<String, String> values = {};

  @override
  Future<String?> read(String key) async => values[key];

  @override
  Future<void> write(String key, String value) async => values[key] = value;

  @override
  Future<void> delete(String key) async => values.remove(key);
}

/// One string document, read and written whole.
abstract class DocumentStore {
  Future<String?> read();
  Future<void> write(String contents);
}

/// A document held in a single platform-keystore item under [key].
class SecureDocumentStore implements DocumentStore {
  SecureDocumentStore(this.key)
      : _storage = const FlutterSecureStorage(
          mOptions: MacOsOptions(useDataProtectionKeyChain: false),
        );

  final String key;
  final FlutterSecureStorage _storage;

  @override
  Future<String?> read() => _storage.read(key: key);

  @override
  Future<void> write(String contents) =>
      _storage.write(key: key, value: contents);
}

/// In-memory document for tests.
class MemoryDocumentStore implements DocumentStore {
  MemoryDocumentStore([this.contents]);

  String? contents;
  int reads = 0;
  int writes = 0;

  @override
  Future<String?> read() async {
    reads += 1;
    return contents;
  }

  @override
  Future<void> write(String value) async {
    writes += 1;
    contents = value;
  }
}

/// Key/value store held as one JSON object in a [DocumentStore].
///
/// The backing document is read at most once and cached; writes update the
/// cache and rewrite the whole document. When the first read fails (for
/// example, the user denies keychain access), reads degrade to null and
/// writes throw, so a denied store is never overwritten with an empty
/// document.
class JsonDocumentStore implements SecretStore {
  JsonDocumentStore(this._document);

  final DocumentStore _document;
  Map<String, String>? _values;
  bool _loadFailed = false;

  Future<Map<String, String>?> _load() async {
    if (_values != null || _loadFailed) {
      return _values;
    }
    final String? raw;
    try {
      raw = await _document.read();
    } catch (_) {
      _loadFailed = true;
      return null;
    }
    final values = <String, String>{};
    if (raw != null) {
      try {
        final decoded = jsonDecode(raw) as Map<String, dynamic>;
        decoded.forEach((key, value) {
          if (value is String) {
            values[key] = value;
          }
        });
      } catch (_) {
        // An unreadable document starts over empty.
      }
    }
    return _values = values;
  }

  Future<void> _persist() async {
    await _document.write(jsonEncode(_values));
  }

  @override
  Future<String?> read(String key) async => (await _load())?[key];

  @override
  Future<void> write(String key, String value) async {
    final values = await _load();
    if (values == null) {
      throw StateError('document store unavailable');
    }
    if (values[key] == value) {
      return;
    }
    values[key] = value;
    await _persist();
  }

  @override
  Future<void> delete(String key) async {
    final values = await _load();
    if (values == null || !values.containsKey(key)) {
      return;
    }
    values.remove(key);
    await _persist();
  }
}
