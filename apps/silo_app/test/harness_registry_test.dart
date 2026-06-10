import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/harness_connection.dart';
import 'package:silo_app/src/connection/harness_registry.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';
import 'package:silo_app/src/connection/message_channel.dart';
import 'package:silo_app/src/connection/secret_store.dart';

HarnessRegistry registry({
  SecretStore? secrets,
  SecretStore? settings,
  SecretStore? legacySecrets,
}) =>
    HarnessRegistry(
      secrets: secrets ?? MemorySecretStore(),
      settings: settings ?? MemorySecretStore(),
      legacySecrets: legacySecrets,
    );

void main() {
  group('silo path persistence', () {
    test('setSiloPath stores the path and load restores it', () async {
      final settings = MemorySecretStore();
      final first = registry(settings: settings);
      await first.load();
      expect(first.siloPath, isNull);

      await first.setSiloPath('/usr/local/bin/silo');
      expect(first.siloPath, '/usr/local/bin/silo');
      expect(settings.values['silo/silo-path'], '/usr/local/bin/silo');

      final reloaded = registry(settings: settings);
      await reloaded.load();
      expect(reloaded.siloPath, '/usr/local/bin/silo');
    });

    test('setSiloPath trims the path before storing it', () async {
      final settings = MemorySecretStore();
      final reg = registry(settings: settings);
      await reg.load();

      await reg.setSiloPath('  /usr/local/bin/silo  ');
      expect(reg.siloPath, '/usr/local/bin/silo');
      expect(settings.values['silo/silo-path'], '/usr/local/bin/silo');
    });

    test('setSiloPath with an empty value deletes the stored path',
        () async {
      final settings = MemorySecretStore();
      final reg = registry(settings: settings);
      await reg.load();
      await reg.setSiloPath('/usr/local/bin/silo');

      await reg.setSiloPath('   ');
      expect(reg.siloPath, isNull);
      expect(settings.values.containsKey('silo/silo-path'), isFalse);

      final reloaded = registry(settings: settings);
      await reloaded.load();
      expect(reloaded.siloPath, isNull);
    });
  });

  group('last launch form persistence', () {
    test('setLastLaunchForm stores the form and load restores it', () async {
      final settings = MemorySecretStore();
      final reg = registry(settings: settings);
      await reg.load();
      expect(reg.lastLaunchForm, isNull);

      const form = LocalHarnessFormState(
        workspaceDir: '/tmp/ws',
        siloPath: '/usr/local/bin/silo',
        backend: LlmBackendChoice.openai,
        model: 'gpt-5',
        apiKeyEnv: 'OPENAI_API_KEY',
        sandbox: SandboxChoice.mock,
        domainsText: 'api.example.com\n*.docs.example.com',
        readAllowlistText: '/opt/sdk',
        quotaText: '2.5',
      );
      await reg.setLastLaunchForm(form);
      expect(reg.lastLaunchForm, same(form));
      expect(settings.values, contains('silo/last-launch-options'));

      final reloaded = registry(settings: settings);
      await reloaded.load();
      final restored = reloaded.lastLaunchForm!;
      expect(restored.workspaceDir, form.workspaceDir);
      expect(restored.siloPath, form.siloPath);
      expect(restored.backend, form.backend);
      expect(restored.model, form.model);
      expect(restored.apiKeyEnv, form.apiKeyEnv);
      expect(restored.sandbox, form.sandbox);
      expect(restored.domainsText, form.domainsText);
      expect(restored.readAllowlistText, form.readAllowlistText);
      expect(restored.quotaText, form.quotaText);
    });

    test('load drops an unreadable last-launch document', () async {
      final settings = MemorySecretStore();
      settings.values['silo/last-launch-options'] = 'not json';
      final reg = registry(settings: settings);
      await reg.load();
      expect(reg.lastLaunchForm, isNull);
      expect(reg.loaded, isTrue);
    });

    test('preference persistence survives broken stores', () async {
      final reg = HarnessRegistry(
        secrets: _ThrowingSecretStore(),
        settings: _ThrowingSecretStore(),
        legacySecrets: _ThrowingSecretStore(),
      );

      // Stores that reject every operation (e.g. a denied keychain) must
      // not crash loading or preference saves; the values stay usable in
      // memory for the session.
      await reg.load();
      expect(reg.loaded, isTrue);

      await reg.setSiloPath('/opt/silo/bin/silo');
      expect(reg.siloPath, '/opt/silo/bin/silo');

      final form = LocalHarnessFormState(
        workspaceDir: '/tmp/ws',
        siloPath: '/opt/silo/bin/silo',
        backend: LlmBackendChoice.anthropic,
        model: 'claude-sonnet-4-6',
        apiKeyEnv: 'ANTHROPIC_API_KEY',
        sandbox: SandboxChoice.auto,
        domainsText: '',
        readAllowlistText: '',
        quotaText: '',
      );
      await reg.setLastLaunchForm(form);
      expect(reg.lastLaunchForm, same(form));
    });
  });

  group('endpoint dedup', () {
    test('addLocal with a known harness id updates the existing entry',
        () async {
      final secrets = MemorySecretStore();
      final settings = MemorySecretStore();
      final reg = registry(secrets: secrets, settings: settings);
      await reg.load();

      await reg.addLocal(
        name: 'ws-a',
        url: 'wss://127.0.0.1:7777',
        token: 'tok-1',
        fingerprintSha256: 'AA',
        harnessId: 'h-1',
      );
      expect(reg.endpoints, hasLength(1));
      final original = reg.endpoints.single;

      // The same harness restarted on a new port: same id, new URL.
      await reg.addLocal(
        name: 'ws-a',
        url: 'wss://127.0.0.1:8888',
        token: 'tok-2',
        fingerprintSha256: 'BB',
        harnessId: 'h-1',
      );
      expect(reg.endpoints, hasLength(1));
      final updated = reg.endpoints.single;
      expect(updated.id, original.id);
      expect(updated.url, 'wss://127.0.0.1:8888');
      expect(secrets.values[updated.tokenKey], 'tok-2');
      expect(settings.values[updated.fingerprintKey], 'bb');
    });

    test('addLocal with a known URL updates instead of appending', () async {
      final reg = registry();
      await reg.load();
      await reg.addLocal(
        name: 'first',
        url: 'wss://127.0.0.1:7777',
        token: 't',
        fingerprintSha256: 'aa',
        harnessId: 'h-1',
      );
      await reg.addLocal(
        name: 'second',
        url: 'wss://127.0.0.1:7777',
        token: 't',
        fingerprintSha256: 'aa',
        harnessId: 'h-2',
      );
      expect(reg.endpoints, hasLength(1));
      expect(reg.endpoints.single.name, 'second');
      expect(reg.endpoints.single.harnessId, 'h-2');
    });

    test('addPaired with a known URL updates instead of appending', () async {
      final reg = registry();
      await reg.load();
      await reg.addPaired(
        name: 'phone-target',
        url: 'wss://10.0.0.5:7777',
        pairingCode: 'AAAA1111',
      );
      final connection = await reg.addPaired(
        name: 'phone-target-2',
        url: 'wss://10.0.0.5:7777',
        pairingCode: 'BBBB2222',
      );
      expect(reg.endpoints, hasLength(1));
      expect(reg.endpoints.single.name, 'phone-target-2');
      expect(connection.pendingPairingCode, 'BBBB2222');
    });

    test('load dedupes a persisted list, preferring the credentialed entry',
        () async {
      final secrets = MemorySecretStore();
      final settings = MemorySecretStore();
      settings.values['silo/storage-migrated'] = '1';
      settings.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'old', 'url': 'wss://a:1', 'harness_id': 'h-1'},
        {'id': 'e2', 'name': 'other', 'url': 'wss://b:2'},
        {'id': 'e3', 'name': 'new', 'url': 'wss://a:9', 'harness_id': 'h-1'},
      ]);
      // Only the older duplicate has stored credentials, so it wins over
      // the newer one.
      secrets.values['silo/e1/token'] = 'tok';

      final reg = registry(secrets: secrets, settings: settings);
      await reg.load();
      expect(reg.endpoints.map((e) => e.id).toList(), ['e1', 'e2']);
      // The losing duplicate's values are deleted and the pruned list
      // persists.
      expect(secrets.values['silo/e1/token'], 'tok');
      expect(secrets.values.containsKey('silo/e3/token'), isFalse);
      final persisted =
          jsonDecode(settings.values['silo/registry']!) as List<dynamic>;
      expect(persisted, hasLength(2));
    });

    test('load dedup keeps the most recently added when both have credentials',
        () async {
      final secrets = MemorySecretStore();
      final settings = MemorySecretStore();
      settings.values['silo/storage-migrated'] = '1';
      settings.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'old', 'url': 'wss://a:1', 'harness_id': 'h-1'},
        {'id': 'e3', 'name': 'new', 'url': 'wss://a:9', 'harness_id': 'h-1'},
      ]);
      secrets.values['silo/e1/token'] = 'old-tok';
      secrets.values['silo/e3/key_seed'] = 'seed';

      final reg = registry(secrets: secrets, settings: settings);
      await reg.load();
      expect(reg.endpoints.single.id, 'e3');
      expect(secrets.values.containsKey('silo/e1/token'), isFalse);
      expect(secrets.values['silo/e3/key_seed'], 'seed');
    });

    test('dedup merges the harness id onto a credentialed winner without one',
        () async {
      final secrets = MemorySecretStore();
      final settings = MemorySecretStore();
      settings.values['silo/storage-migrated'] = '1';
      settings.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'old', 'url': 'wss://a:1'},
        {'id': 'e2', 'name': 'new', 'url': 'wss://a:1', 'harness_id': 'h-1'},
      ]);
      secrets.values['silo/e1/token'] = 'tok';

      final reg = registry(secrets: secrets, settings: settings);
      await reg.load();
      expect(reg.endpoints.single.id, 'e1');
      expect(reg.endpoints.single.harnessId, 'h-1');
    });

    test('load dedupes entries with the same URL', () async {
      final settings = MemorySecretStore();
      settings.values['silo/storage-migrated'] = '1';
      settings.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'old', 'url': 'wss://a:1'},
        {'id': 'e2', 'name': 'new', 'url': 'wss://a:1'},
      ]);
      final reg = registry(settings: settings);
      await reg.load();
      expect(reg.endpoints.single.id, 'e2');
    });

    test('an open connection displaced by a URL update is disposed only '
        'after it disconnects', () async {
      // The channel factory never completes, so a connect attempt parks
      // the connection in the connecting state.
      final reg = HarnessRegistry(
        secrets: MemorySecretStore(),
        settings: MemorySecretStore(),
        channelFactory: (uri, fingerprint) =>
            Completer<MessageChannel>().future,
      );
      await reg.load();
      final first = await reg.addLocal(
        name: 'ws',
        url: 'wss://a:1',
        token: 't',
        fingerprintSha256: 'aa',
        harnessId: 'h-1',
      );
      unawaited(first.connect());
      await pumpEventQueue();
      expect(first.status, ConnectionStatus.connecting);

      // The same harness reappears on a new port; the entry updates and a
      // fresh connection replaces the old one.
      final second = await reg.addLocal(
        name: 'ws',
        url: 'wss://a:2',
        token: 't',
        fingerprintSha256: 'aa',
        harnessId: 'h-1',
      );
      expect(identical(first, second), isFalse);
      expect(reg.endpoints, hasLength(1));

      // The displaced connection is still open, so it is not disposed yet.
      expect(() => first.addListener(() {}), returnsNormally);

      await first.disconnect();
      await pumpEventQueue();
      // Disconnected, so the deferred disposal has run.
      expect(() => first.addListener(() {}), throwsFlutterError);
      reg.dispose();
    });
  });

  group('legacy storage migration', () {
    test('moves legacy keychain items into the consolidated layout',
        () async {
      final legacy = MemorySecretStore();
      legacy.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'one', 'url': 'wss://a:1'},
      ]);
      legacy.values['silo/e1/token'] = 'tok';
      legacy.values['silo/e1/key_seed'] = 'seed';
      legacy.values['silo/e1/key_id'] = 'key-1';
      legacy.values['silo/e1/fingerprint'] = 'ff';
      legacy.values['silo/silo-path'] = '/usr/local/bin/silo';
      legacy.values['silo/last-launch-options'] =
          jsonEncode(const LocalHarnessFormState(
        workspaceDir: '/tmp/ws',
        siloPath: '/usr/local/bin/silo',
        backend: LlmBackendChoice.anthropic,
        model: 'm',
        apiKeyEnv: 'K',
        sandbox: SandboxChoice.auto,
        domainsText: '',
        readAllowlistText: '',
        quotaText: '',
      ).toJson());

      final secrets = MemorySecretStore();
      final settings = MemorySecretStore();
      final reg = registry(
        secrets: secrets,
        settings: settings,
        legacySecrets: legacy,
      );
      await reg.load();

      // Real secrets land in the consolidated secret store.
      expect(secrets.values['silo/e1/token'], 'tok');
      expect(secrets.values['silo/e1/key_seed'], 'seed');
      // Everything else lands in the preferences store.
      expect(settings.values['silo/e1/key_id'], 'key-1');
      expect(settings.values['silo/e1/fingerprint'], 'ff');
      expect(settings.values['silo/silo-path'], '/usr/local/bin/silo');
      expect(settings.values, contains('silo/registry'));
      expect(settings.values, contains('silo/last-launch-options'));
      expect(settings.values['silo/storage-migrated'], '1');
      // The legacy items are gone.
      expect(legacy.values, isEmpty);

      expect(reg.endpoints.single.id, 'e1');
      expect(reg.siloPath, '/usr/local/bin/silo');
      expect(reg.lastLaunchForm, isNotNull);
    });

    test('runs once: the migrated flag stops later legacy probes', () async {
      final legacy = MemorySecretStore();
      legacy.values['silo/registry'] = jsonEncode([
        {'id': 'e1', 'name': 'one', 'url': 'wss://a:1'},
      ]);
      final settings = MemorySecretStore();
      settings.values['silo/storage-migrated'] = '1';

      final reg = registry(settings: settings, legacySecrets: legacy);
      await reg.load();
      // No migration: the flag was already set.
      expect(reg.endpoints, isEmpty);
      expect(legacy.values, contains('silo/registry'));
    });

    test('a fresh install sets the flag without needing legacy data',
        () async {
      final settings = MemorySecretStore();
      final reg = registry(
        settings: settings,
        legacySecrets: MemorySecretStore(),
      );
      await reg.load();
      expect(settings.values['silo/storage-migrated'], '1');
      expect(reg.endpoints, isEmpty);
    });

    test('tolerates a legacy store that denies every read', () async {
      final settings = MemorySecretStore();
      final reg = registry(
        settings: settings,
        legacySecrets: _ThrowingSecretStore(),
      );
      await reg.load();
      expect(reg.loaded, isTrue);
      expect(settings.values['silo/storage-migrated'], '1');
    });
  });

  group('consolidated secret document', () {
    test('reads the backing document at most once', () async {
      final document = MemoryDocumentStore(jsonEncode({'a': '1', 'b': '2'}));
      final store = JsonDocumentStore(document);
      expect(await store.read('a'), '1');
      expect(await store.read('b'), '2');
      expect(await store.read('missing'), isNull);
      expect(document.reads, 1);
    });

    test('writes through to the backing document', () async {
      final document = MemoryDocumentStore();
      final store = JsonDocumentStore(document);
      await store.write('k', 'v');
      expect(jsonDecode(document.contents!), {'k': 'v'});
      await store.delete('k');
      expect(jsonDecode(document.contents!), isEmpty);
    });

    test('a failed load never overwrites the backing document', () async {
      final document = _ThrowingDocumentStore();
      final store = JsonDocumentStore(document);
      expect(await store.read('k'), isNull);
      await expectLater(store.write('k', 'v'), throwsStateError);
    });
  });
}

/// Secret store whose every operation fails, like a keystore without the
/// required platform entitlement.
class _ThrowingSecretStore implements SecretStore {
  @override
  Future<String?> read(String key) async {
    throw Exception('secret store unavailable');
  }

  @override
  Future<void> write(String key, String value) async {
    throw Exception('secret store unavailable');
  }

  @override
  Future<void> delete(String key) async {
    throw Exception('secret store unavailable');
  }
}

class _ThrowingDocumentStore implements DocumentStore {
  @override
  Future<String?> read() async => throw Exception('denied');

  @override
  Future<void> write(String contents) async => throw Exception('denied');
}
