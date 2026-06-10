import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/local_harness_options.dart';
import 'package:silo_app/src/ui/home_screen.dart';

void main() {
  /// Pumps a host app with a button that opens [StartLocalDialog] and
  /// records the popped options into [results]. The silo binary resolves
  /// to `/usr/local/bin/silo` and every path exists, unless overridden.
  /// [initialForm] is re-evaluated on each opening, so a test can feed the
  /// state captured by [onFormChanged] back into the next dialog.
  Future<void> pumpHost(
    WidgetTester tester,
    List<LocalHarnessOptions?> results, {
    required Future<String?> Function() pickDirectory,
    Future<bool> Function(String dir)? isWorkspaceLocked,
    String? Function(String? configuredPath)? resolveSilo,
    bool Function(String path)? siloExists,
    String? initialSiloPath,
    LocalHarnessFormState? Function()? initialForm,
    ValueChanged<LocalHarnessFormState>? onFormChanged,
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
                resolveSilo: resolveSilo ?? (_) => '/usr/local/bin/silo',
                siloExists: siloExists ?? (_) => true,
                initialSiloPath: initialSiloPath,
                initialForm: initialForm?.call(),
                onFormChanged: onFormChanged,
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

  /// The current text of the [TextField] labelled [label].
  String fieldText(WidgetTester tester, String label) => tester
      .widget<TextField>(find.widgetWithText(TextField, label))
      .controller!
      .text;

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
    expect(options.siloBinary, '/usr/local/bin/silo');
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

  testWidgets('prefills the silo binary field with the resolved path',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/ws',
      resolveSilo: (configured) => configured ?? '/opt/homebrew/bin/silo',
    );

    expect(find.text('/opt/homebrew/bin/silo'), findsOneWidget);

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();

    // The composed command uses the resolved absolute path, not "silo".
    expect(
      find.textContaining('/opt/homebrew/bin/silo run --workspace /tmp/ws'),
      findsOneWidget,
    );
  });

  testWidgets('an edited silo path lands in the composed command',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(tester, results, pickDirectory: () async => '/tmp/ws');

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();

    await tester.enterText(
      find.widgetWithText(TextField, 'silo binary'),
      '/repo/target/release/silo',
    );
    await tester.pumpAndSettle();

    expect(
      find.textContaining(
          '/repo/target/release/silo run --workspace /tmp/ws'),
      findsOneWidget,
    );

    await tester.tap(find.widgetWithText(FilledButton, 'Start'));
    await tester.pumpAndSettle();

    expect(results.single!.siloBinary, '/repo/target/release/silo');
  });

  testWidgets('prefills every field from a restored form state',
      (tester) async {
    const form = LocalHarnessFormState(
      workspaceDir: '/tmp/restored-ws',
      siloPath: '/saved/bin/silo',
      backend: LlmBackendChoice.openai,
      model: 'gpt-5-custom',
      apiKeyEnv: 'MY_OPENAI_KEY',
      sandbox: SandboxChoice.mock,
      domainsText: 'api.example.com\n*.docs.example.com',
      readAllowlistText: '/opt/sdk\n/usr/share/doc',
      quotaText: '2.5',
    );
    final results = <LocalHarnessOptions?>[];
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/other',
      initialSiloPath: '/older/bin/silo',
      resolveSilo: (configured) => configured,
      initialForm: () => form,
    );

    expect(fieldText(tester, 'Workspace directory'), '/tmp/restored-ws');
    // The restored form's silo path wins over initialSiloPath.
    expect(fieldText(tester, 'silo binary'), '/saved/bin/silo');
    expect(find.text('openai'), findsOneWidget);
    expect(fieldText(tester, 'Model'), 'gpt-5-custom');
    expect(
        fieldText(tester, 'API key environment variable'), 'MY_OPENAI_KEY');
    expect(find.text('mock'), findsOneWidget);
    expect(fieldText(tester, 'Allowed domains (one per line)'),
        'api.example.com\n*.docs.example.com');
    expect(fieldText(tester, 'Read-allowlist paths (one per line)'),
        '/opt/sdk\n/usr/share/doc');
    expect(fieldText(tester, 'Dollar quota (optional)'), '2.5');

    // The restored form is complete, so Start is enabled and pops options
    // built from the restored values.
    await tester.tap(find.widgetWithText(FilledButton, 'Start'));
    await tester.pumpAndSettle();
    final options = results.single!;
    expect(options.workspaceDir, '/tmp/restored-ws');
    expect(options.siloBinary, '/saved/bin/silo');
    expect(options.backend, LlmBackendChoice.openai);
    expect(options.model, 'gpt-5-custom');
    expect(options.apiKeyEnv, 'MY_OPENAI_KEY');
    expect(options.sandbox, SandboxChoice.mock);
    expect(
        options.allowedDomains, ['api.example.com', '*.docs.example.com']);
    expect(options.readAllowlist, ['/opt/sdk', '/usr/share/doc']);
    expect(options.quotaUsd, 2.5);
  });

  testWidgets('a restored model still tracks backend default switching',
      (tester) async {
    // The restored model equals the restored backend's default, so
    // switching backends replaces it, as it does for an untouched field.
    const form = LocalHarnessFormState(
      workspaceDir: '/tmp/ws',
      siloPath: '/usr/local/bin/silo',
      backend: LlmBackendChoice.openai,
      model: 'gpt-5',
      apiKeyEnv: 'OPENAI_API_KEY',
    );
    final results = <LocalHarnessOptions?>[];
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/ws',
      initialForm: () => form,
    );

    await tester.tap(find.text('openai'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('anthropic').last);
    await tester.pumpAndSettle();

    expect(fieldText(tester, 'Model'), 'claude-sonnet-4-6');
    expect(
        fieldText(tester, 'API key environment variable'),
        'ANTHROPIC_API_KEY');
  });

  testWidgets('cancelling preserves the form state for the next opening',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    LocalHarnessFormState? saved;
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/ws',
      initialForm: () => saved,
      onFormChanged: (state) => saved = state,
    );

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();
    await tester.enterText(
        find.widgetWithText(TextField, 'Model'), 'my-model');
    await tester.enterText(
        find.widgetWithText(TextField, 'Allowed domains (one per line)'),
        'api.example.com');
    await tester.enterText(
        find.widgetWithText(TextField, 'Dollar quota (optional)'), '1.5');
    await tester.pumpAndSettle();

    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();

    // The dialog popped without options, and the form state was captured.
    expect(results, [null]);
    expect(saved, isNotNull);
    expect(saved!.workspaceDir, '/tmp/ws');

    await tester.tap(find.text('open'));
    await tester.pumpAndSettle();

    expect(fieldText(tester, 'Workspace directory'), '/tmp/ws');
    expect(fieldText(tester, 'silo binary'), '/usr/local/bin/silo');
    expect(fieldText(tester, 'Model'), 'my-model');
    expect(fieldText(tester, 'Allowed domains (one per line)'),
        'api.example.com');
    expect(fieldText(tester, 'Dollar quota (optional)'), '1.5');
  });

  testWidgets('shows guidance and blocks Start when silo is not found',
      (tester) async {
    final results = <LocalHarnessOptions?>[];
    await pumpHost(
      tester,
      results,
      pickDirectory: () async => '/tmp/ws',
      resolveSilo: (_) => null,
      siloExists: (_) => false,
    );

    expect(find.textContaining('cargo build --release'), findsOneWidget);

    await tester.tap(find.text('Choose…'));
    await tester.pumpAndSettle();

    // Even with a workspace chosen, Start stays disabled.
    expect(
      tester
          .widget<FilledButton>(find.widgetWithText(FilledButton, 'Start'))
          .onPressed,
      isNull,
    );
  });
}
