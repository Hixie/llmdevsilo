/// Storage for client secrets: pairing private keys, local tokens, and
/// pinned certificate fingerprints.
library;

import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// Key/value store for secrets. The app uses [SecureSecretStore] (backed by
/// the platform keystore); tests use [MemorySecretStore].
abstract class SecretStore {
  Future<String?> read(String key);
  Future<void> write(String key, String value);
  Future<void> delete(String key);
}

/// Secrets in the platform-native keystore: Keychain on macOS/iOS, the
/// Android keystore, and browser storage (encrypted with WebCrypto) on web.
class SecureSecretStore implements SecretStore {
  SecureSecretStore() : _storage = const FlutterSecureStorage();

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
