import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';

void main() {
  group('buildRunArgs', () {
    test('minimal options produce a create-locked anthropic run', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
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
          sandbox: choice,
        ));
        final index = args.indexOf('--sandbox');
        expect(args[index + 1], name);
      }
    });

    test('domains expand to repeated --allow-domain flags in order', () {
      final args = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
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
        quotaUsd: 2.5,
      ));
      final index = withQuota.indexOf('--quota-usd');
      expect(index, isNonNegative);
      expect(double.parse(withQuota[index + 1]), 2.5);

      final withoutQuota = buildRunArgs(const LocalHarnessOptions(
        workspaceDir: '/tmp/ws',
      ));
      expect(withoutQuota, isNot(contains('--quota-usd')));
    });
  });

  group('runCommandLine', () {
    test('starts with silo run and quotes spaced values', () {
      final line = runCommandLine(const LocalHarnessOptions(
        workspaceDir: '/tmp/my ws',
        model: 'claude-sonnet-4-6',
      ));
      expect(line, startsWith('silo run --workspace '));
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
