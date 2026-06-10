import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';

void main() {
  group('buildRunArgs', () {
    test('minimal options produce a create-locked anthropic run', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        model: 'claude-sonnet-4-6',
        apiKeyEnv: 'ANTHROPIC_API_KEY',
      ));
      expect(args, [
        'run',
        '--workspace',
        '/tmp/ws',
        '--create',
        '--llm',
        'anthropic',
        '--model',
        'claude-sonnet-4-6',
        '--api-key-env',
        'ANTHROPIC_API_KEY',
        '--sandbox',
        'auto',
      ]);
    });

    test('createWorkspace false omits --create', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        createWorkspace: false,
      ));
      expect(args, isNot(contains('--create')));
    });

    test('each backend maps to its --llm value', () {
      const expected = {
        LlmBackendChoice.anthropic: 'anthropic',
        LlmBackendChoice.openai: 'openai',
        LlmBackendChoice.openaiWs: 'openai-ws',
        LlmBackendChoice.local: 'local',
      };
      for (final backend in LlmBackendChoice.values) {
        final args = buildRunArgs(LocalHarnessOptions(
          workspaceDir: '/tmp/ws',
          siloBinary: '/usr/local/bin/silo',
          backend: backend,
        ));
        final llmIndex = args.indexOf('--llm');
        expect(llmIndex, isNonNegative);
        expect(args[llmIndex + 1], expected[backend], reason: '$backend');
      }
    });

    test('backends carry conventional API key env var defaults', () {
      expect(
          LlmBackendChoice.anthropic.defaultApiKeyEnv, 'ANTHROPIC_API_KEY');
      expect(LlmBackendChoice.openai.defaultApiKeyEnv, 'OPENAI_API_KEY');
      expect(LlmBackendChoice.openaiWs.defaultApiKeyEnv, 'OPENAI_API_KEY');
      expect(LlmBackendChoice.local.defaultApiKeyEnv, isEmpty);
    });

    test('empty model and API key env omit their flags', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        backend: LlmBackendChoice.local,
      ));
      expect(args, isNot(contains('--model')));
      expect(args, isNot(contains('--api-key-env')));
    });

    test('sandbox choices map to --sandbox values', () {
      for (final (choice, name) in [
        (SandboxChoice.auto, 'auto'),
        (SandboxChoice.mock, 'mock'),
      ]) {
        final args = buildRunArgs(LocalHarnessOptions(
          workspaceDir: '/tmp/ws',
          siloBinary: '/usr/local/bin/silo',
          sandbox: choice,
        ));
        final index = args.indexOf('--sandbox');
        expect(args[index + 1], name);
      }
    });

    test('domains expand to repeated --allow-domain flags in order', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        allowedDomains: ['api.example.com', '*.docs.example.com'],
      ));
      expect(
        args.join(' '),
        contains('--allow-domain api.example.com '
            '--allow-domain *.docs.example.com'),
      );
    });

    test('read allowlist expands to repeated --allow-read flags', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        readAllowlist: ['/opt/sdk', '/usr/share/doc'],
      ));
      expect(
        args.join(' '),
        contains('--allow-read /opt/sdk --allow-read /usr/share/doc'),
      );
    });

    test('dollar quota maps to --quota-usd and is omitted when null', () {
      final withQuota = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
        quotaUsd: 2.5,
      ));
      final index = withQuota.indexOf('--quota-usd');
      expect(index, isNonNegative);
      expect(double.parse(withQuota[index + 1]), 2.5);

      final withoutQuota = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
        siloBinary: '/usr/local/bin/silo',
      ));
      expect(withoutQuota, isNot(contains('--quota-usd')));
    });
  });

  group('runCommandLine', () {
    test('starts with the silo binary and quotes spaced values', () {
      final line = runCommandLine(const LocalHarnessOptions(
        workspaceDir: '/tmp/my ws',
        siloBinary: '/usr/local/bin/silo',
        model: 'claude-sonnet-4-6',
      ));
      expect(line, startsWith('/usr/local/bin/silo run --workspace '));
      expect(line, contains("'/tmp/my ws'"));
    });

    test('round-trips single quotes', () {
      expect(shellQuote("it's"), "'it'\\''s'");
      expect(shellQuote('plain-value_1.0'), 'plain-value_1.0');
      expect(shellQuote(''), "''");
    });
  });

  group('splitLines', () {
    test('trims entries and drops blank lines', () {
      expect(
        splitLines('  api.example.com \n\n *.docs.example.com\n'),
        ['api.example.com', '*.docs.example.com'],
      );
      expect(splitLines(''), isEmpty);
    });
  });

  group('LocalHarnessFormState', () {
    test('round-trips through JSON', () {
      const form = LocalHarnessFormState(
        workspaceDir: '/tmp/ws',
        siloPath: '/usr/local/bin/silo',
        backend: LlmBackendChoice.openai,
        model: 'gpt-5',
        apiKeyEnv: 'OPENAI_API_KEY',
        sandbox: SandboxChoice.mock,
        domainsText: 'api.example.com\n*.docs.example.com',
        readAllowlistText: '/opt/sdk\n/usr/share/doc',
        quotaText: '2.5',
      );
      final restored = LocalHarnessFormState.fromJson(
          jsonDecode(jsonEncode(form.toJson())) as Map<String, dynamic>);
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

    test('missing or unknown values fall back to the defaults', () {
      final restored = LocalHarnessFormState.fromJson({
        'backend': 'no-such-backend',
        'sandbox': 'no-such-sandbox',
      });
      expect(restored.workspaceDir, '');
      expect(restored.siloPath, '');
      expect(restored.backend, LlmBackendChoice.anthropic);
      expect(restored.sandbox, SandboxChoice.auto);
      expect(restored.quotaText, '');
    });
  });

  group('HarnessStartError', () {
    test('includes the stderr tail in its message when present', () {
      expect(HarnessStartError('boom').toString(), 'boom');
      expect(
        HarnessStartError('boom', stderrTail: 'detail').toString(),
        'boom\ndetail',
      );
    });
  });
}
