import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';
import 'package:silo_app/src/ui/home_screen.dart';

void main() {
  /// Pumps a host app with a button that opens [StartLocalDialog] and
  /// records the popped options into [results].
  Future<void> pumpHost(
    WidgetTester tester,
    List<LocalHarnessOptions?> results, {
    required Future<String?> Function() pickDirectory,
    Future<bool> Function(String dir)? isWorkspaceLocked,
  }) async {
    await tester.pumpWidget(MaterialApp(
      home: Builder(
        builder: (context) => TextButton(
          onPressed: () async {
            results.add(await showDialog<LocalHarnessOptions>(
              context: context,
              builder: (_) => StartLocalDialog(
                pickDirectory: pickDirectory,
                isWorkspaceLocked: isWorkspaceLocked ?? (_) async => false,
              ),
            ));
          },
          child: const Text('open'),
        ),
      ),
    ));
    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();
  }

  testWidgets('composes options from the form fields', (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(tester, results, pickDirectory: () async => '/tmp/ws');

    expect(find.text('Start a local harness'), findsOneWidget);
    // The Start button is disabled until a workspace directory is chosen.
    expect(
      tester
          .widget<FilledButton>(find.widgetWithText(FilledButton, 'Start'))
          .onPressed,
      isNull,
    );

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();
    expect(find.text('/tmp/ws'), findsOneWidget);

    await tester.enterText(
      find.widgetWithText(TextField, 'Allowed domains (one per line)'),
      'api.example.com\n*.docs.example.com',
    );
    await tester.enterText(
      find.widgetWithText(TextField, 'Read-allowlist paths (one per line)'),
      '/opt/sdk',
    );
    await tester.enterText(
      find.widgetWithText(TextField, 'Dollar quota (optional)'),
      '2.5',
    );
    await tester.pumpAndSettle();

    // The composed command line is shown for copy-paste.
    expect(
      find.textContaining('silo run --workspace /tmp/ws --create'),
      findsOneWidget,
    );

    await tester.tap(find.widgetWithText(FilledButton, 'Start'));
    await tester.pumpAndSettle();

    expect(results, hasLength(1));
    final options = results.single!;
    expect(options.workspaceDir, '/tmp/ws');
    expect(options.createWorkspace, isTrue);
    expect(options.backend, LlmBackendChoice.anthropic);
    expect(options.model, 'claude-sonnet-4-6');
    expect(options.apiKeyEnv, 'ANTHROPIC_API_KEY');
    expect(options.sandbox, SandboxChoice.auto);
    expect(
        options.allowedDomains, ['api.example.com', '*.docs.example.com']);
    expect(options.readAllowlist, ['/opt/sdk']);
    expect(options.quotaUsd, 2.5);
  });

  testWidgets('switching backend updates the default model and key env',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(tester, results, pickDirectory: () async => '/tmp/ws');

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();

    await tester.tap(find.text('anthropic'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('openai').last);
    await tester.pumpAndSettle();

    expect(find.text('gpt-5'), findsOneWidget);
    expect(find.text('OPENAI_API_KEY'), findsOneWidget);

    await tester.tap(find.widgetWithText(FilledButton, 'Start'));
    await tester.pumpAndSettle();

    final options = results.single!;
    expect(options.backend, LlmBackendChoice.openai);
    expect(options.model, 'gpt-5');
    expect(options.apiKeyEnv, 'OPENAI_API_KEY');
  });

  testWidgets('a locked workspace drops --create from the command',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/locked-ws',
      isWorkspaceLocked: (dir) async => dir == '/tmp/locked-ws',
    );

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();

    expect(find.textContaining('--create'), findsNothing);

    await tester.tap(find.widgetWithText(FilledButton, 'Start'));
    await tester.pumpAndSettle();

    expect(results.single!.createWorkspace, isFalse);
  });
}
