import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/harness_registry.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';
import 'package:silo_app/src/connection/secret_store.dart';

void main() {
  group('silo path persistence', () {
    test('setSiloPath stores the path and load restores it', () async {
      final secrets = MemorySecretStore();
      final registry = HarnessRegistry(secrets: secrets);
      await registry.load();
      expect(registry.siloPath, isNull);

      await registry.setSiloPath('/usr/local/bin/silo');
      expect(registry.siloPath, '/usr/local/bin/silo');
      expect(secrets.values['silo/silo-path'], '/usr/local/bin/silo');

      final reloaded = HarnessRegistry(secrets: secrets);
      await reloaded.load();
      expect(reloaded.siloPath, '/usr/local/bin/silo');
    });

    test('setSiloPath trims the path before storing it', () async {
      final secrets = MemorySecretStore();
      final registry = HarnessRegistry(secrets: secrets);
      await registry.load();

      await registry.setSiloPath('  /usr/local/bin/silo  ');
      expect(registry.siloPath, '/usr/local/bin/silo');
      expect(secrets.values['silo/silo-path'], '/usr/local/bin/silo');
    });

    test('setSiloPath with an empty value deletes the stored path',
        () async {
      final secrets = MemorySecretStore();
      final registry = HarnessRegistry(secrets: secrets);
      await registry.load();
      await registry.setSiloPath('/usr/local/bin/silo');

      await registry.setSiloPath('   ');
      expect(registry.siloPath, isNull);
      expect(secrets.values.containsKey('silo/silo-path'), isFalse);

      final reloaded = HarnessRegistry(secrets: secrets);
      await reloaded.load();
      expect(reloaded.siloPath, isNull);
    });
  });

  group('last launch form persistence', () {
    test('setLastLaunchForm stores the form and load restores it', () async {
      final secrets = MemorySecretStore();
      final registry = HarnessRegistry(secrets: secrets);
      await registry.load();
      expect(registry.lastLaunchForm, isNull);

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
      await registry.setLastLaunchForm(form);
      expect(registry.lastLaunchForm, same(form));
      expect(secrets.values, contains('silo/last-launch-options'));

      final reloaded = HarnessRegistry(secrets: secrets);
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
      final secrets = MemorySecretStore();
      secrets.values['silo/last-launch-options'] = 'not json';
      final registry = HarnessRegistry(secrets: secrets);
      await registry.load();
      expect(registry.lastLaunchForm, isNull);
      expect(registry.loaded, isTrue);
    });
  });
}
