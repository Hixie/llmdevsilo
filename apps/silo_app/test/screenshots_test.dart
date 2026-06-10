// Renders the chat screen, with the question card pinned over a populated
// transcript, at two window sizes, plus the pairing sheet, and asserts
// that no render overflow occurred. When the SILO_SCREENSHOTS environment
// variable is set, also writes PNG previews to build/ui-previews/.

import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/event_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';
import 'package:silo_app/src/ui/chat_view.dart';
import 'package:silo_app/src/ui/pairing_sheet.dart';
import 'package:silo_app/src/ui/question_card.dart';
import 'package:silo_app/src/ui/theme.dart';

final bool _writeScreenshots =
    Platform.environment['SILO_SCREENSHOTS'] != null;

const Key _boundaryKey = ValueKey('screenshot-boundary');

/// Loads the bundled fonts so rendered frames use Inter and JetBrains Mono
/// instead of the test-default font.
Future<void> _loadFonts() async {
  Future<ByteData> read(String path) async {
    final bytes = await File(path).readAsBytes();
    return ByteData.sublistView(bytes);
  }

  final inter = FontLoader('Inter')
    ..addFont(read('assets/fonts/Inter-Regular.ttf'))
    ..addFont(read('assets/fonts/Inter-Medium.ttf'))
    ..addFont(read('assets/fonts/Inter-SemiBold.ttf'))
    ..addFont(read('assets/fonts/Inter-Bold.ttf'));
  final mono = FontLoader('JetBrains Mono')
    ..addFont(read('assets/fonts/JetBrainsMono-Regular.ttf'));
  await inter.load();
  await mono.load();

  // The icon font ships with the SDK rather than the app; load it so icons
  // render in screenshots instead of placeholder boxes.
  final flutterRoot = Platform.environment['FLUTTER_ROOT'];
  if (flutterRoot != null) {
    final icons = File('$flutterRoot/bin/cache/artifacts/material_fonts/'
        'MaterialIcons-Regular.otf');
    if (icons.existsSync()) {
      final loader = FontLoader('MaterialIcons')
        ..addFont(read(icons.path));
      await loader.load();
    }
  }
}

Event _event(int seq, EventPayload payload) =>
    Event(seq: seq, time: Timestamp(logical: seq), payload: payload);

EventStore _demoStore() {
  final store = EventStore();
  store.insertAll([
    _event(0, const HarnessStartedPayload(
      harnessId: 'h-demo',
      workspace: '/work/llmdevsilo',
      sandbox: 'firejail',
      llm: 'anthropic',
    )),
    _event(1, const UserPromptPayload(
      clientId: 'c-1',
      text: 'Add retry handling to the websocket reconnect path.',
    )),
    _event(2, const AssistantTextPayload(
      agent: 'agent-0',
      text: 'Looking at the reconnect logic now. The current backoff is '
          'fixed at one second, which hammers the server when it is '
          'restarting; an exponential schedule with jitter would behave '
          'better under sustained outages.',
    )),
    _event(3, const ToolUsePayload(
      agent: 'agent-0',
      call: ToolCall(
        id: 't-1',
        name: 'Bash',
        input: {'command': 'grep -rn "reconnect" crates/silo-net/src/'},
      ),
    )),
    _event(4, const ToolResultPayload(
      agent: 'agent-0',
      toolUseId: 't-1',
      toolName: 'Bash',
      output: ToolOutput(
        content: 'crates/silo-net/src/client.rs:88: fn reconnect(&mut self)',
        isError: false,
      ),
    )),
    _event(5, const QuestionAskedPayload(
      id: 'q-1',
      agent: 'agent-0',
      question: UserQuestion(
        question: 'Which backoff strategy should the reconnect loop use?',
        options: [
          QuestionOption(
            label: 'Exponential with jitter',
            description: 'Doubles the delay each attempt, randomized ±25%',
          ),
          QuestionOption(
            label: 'Exponential, no jitter',
            description: 'Doubles the delay each attempt, deterministic',
          ),
          QuestionOption(
            label: 'Fixed 5 second interval',
            description: 'Simple, but synchronizes clients after an outage',
          ),
          QuestionOption(
            label: 'Fibonacci backoff',
            description: 'Grows more slowly than exponential',
          ),
          QuestionOption(
            label: 'Keep the current behavior',
            description: 'Retry every second indefinitely',
          ),
        ],
        allowFreeText: true,
      ),
    )),
  ]);
  return store;
}

Widget _chatScaffold(EventStore store) {
  return RepaintBoundary(
    key: _boundaryKey,
    child: MaterialApp(
      debugShowCheckedModeBanner: false,
      theme: siloTheme(Brightness.light),
      home: Scaffold(
        appBar: AppBar(title: const Text('demo harness')),
        body: Column(
          children: [
            Expanded(
              child: ChatView(store: store, onAnswer: (_, _) {}),
            ),
            // Stand-in for the input row of ChatScreen, which needs a live
            // connection.
            Builder(builder: (context) {
              final scheme = Theme.of(context).colorScheme;
              return Container(
                padding: const EdgeInsets.fromLTRB(8, 6, 8, 8),
                decoration: BoxDecoration(
                  color: scheme.surfaceContainerLow,
                  border:
                      Border(top: BorderSide(color: scheme.outlineVariant)),
                ),
                child: Row(
                  children: [
                    const IconButton(
                        icon: Icon(Icons.attach_file), onPressed: null),
                    const Expanded(
                      child: TextField(
                        decoration: InputDecoration(
                          hintText: 'Message the harness',
                          border: OutlineInputBorder(
                            borderRadius:
                                BorderRadius.all(Radius.circular(24)),
                          ),
                          isDense: true,
                          contentPadding: EdgeInsets.symmetric(
                              horizontal: 16, vertical: 10),
                        ),
                      ),
                    ),
                    const SizedBox(width: 6),
                    IconButton.filled(
                        icon: const Icon(Icons.arrow_upward),
                        onPressed: () {}),
                  ],
                ),
              );
            }),
          ],
        ),
      ),
    ),
  );
}

Future<void> _capture(WidgetTester tester, String name) async {
  if (!_writeScreenshots) {
    return;
  }
  final boundary = tester.renderObject(find.byKey(_boundaryKey))
      as RenderRepaintBoundary;
  await tester.runAsync(() async {
    final image = await boundary.toImage();
    final data = await image.toByteData(format: ui.ImageByteFormat.png);
    final dir = Directory('build/ui-previews')..createSync(recursive: true);
    final file = File('${dir.path}/$name.png');
    file.writeAsBytesSync(data!.buffer.asUint8List());
    // The absolute path, for whoever collects the previews.
    debugPrint('wrote ${file.absolute.path}');
  });
}

Future<void> _pumpAt(WidgetTester tester, Size size) async {
  tester.view.physicalSize = size;
  tester.view.devicePixelRatio = 1.0;
  addTearDown(tester.view.reset);
  await tester.pumpWidget(_chatScaffold(_demoStore()));
  await tester.pump();
  await tester.pump();
}

void main() {
  setUpAll(_loadFonts);

  for (final (size, name) in [
    (const Size(900, 700), 'chat_question_card_900x700'),
    (const Size(900, 420), 'chat_question_card_900x420'),
    // Narrow window: text and tool payloads must wrap, not overflow.
    (const Size(360, 640), 'chat_question_card_360x640'),
  ]) {
    testWidgets(
        'chat with pinned question card renders without overflow at '
        '${size.width.toInt()}x${size.height.toInt()}', (tester) async {
      final overflowErrors = <FlutterErrorDetails>[];
      final originalOnError = FlutterError.onError;
      FlutterError.onError = (details) {
        if (details.exceptionAsString().contains('overflowed')) {
          overflowErrors.add(details);
        } else {
          originalOnError?.call(details);
        }
      };
      addTearDown(() => FlutterError.onError = originalOnError);

      await _pumpAt(tester, size);

      expect(find.byType(QuestionCard), findsOneWidget);
      expect(
        find.text('Which backoff strategy should the reconnect loop use?'),
        findsWidgets,
      );
      await _capture(tester, name);

      // At the tall size, also capture the card scrolled to its end, which
      // shows the free-text section below the options.
      if (size.height >= 700) {
        final cardPosition = tester
            .state<ScrollableState>(find
                .descendant(
                    of: find.byType(QuestionCard),
                    matching: find.byType(Scrollable))
                .first)
            .position;
        cardPosition.jumpTo(cardPosition.maxScrollExtent);
        await tester.pump();
        await _capture(tester, '${name}_scrolled');
      }

      expect(
        overflowErrors,
        isEmpty,
        reason: 'render overflow at ${size.width}x${size.height}: '
            '${overflowErrors.map((e) => e.exceptionAsString()).join('; ')}',
      );
    });
  }

  testWidgets('pairing sheet renders without overflow at 900x700',
      (tester) async {
    final overflowErrors = <FlutterErrorDetails>[];
    final originalOnError = FlutterError.onError;
    FlutterError.onError = (details) {
      if (details.exceptionAsString().contains('overflowed')) {
        overflowErrors.add(details);
      } else {
        originalOnError?.call(details);
      }
    };
    addTearDown(() => FlutterError.onError = originalOnError);

    tester.view.physicalSize = const Size(900, 700);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    await tester.pumpWidget(RepaintBoundary(
      key: _boundaryKey,
      child: MaterialApp(
        debugShowCheckedModeBanner: false,
        theme: siloTheme(Brightness.light),
        home: Scaffold(
          appBar: AppBar(title: const Text('demo harness')),
          body: Builder(
            builder: (context) => Center(
              child: FilledButton(
                onPressed: () => showModalBottomSheet<void>(
                  context: context,
                  isScrollControlled: true,
                  showDragHandle: true,
                  builder: (_) => PairingSheet(
                    url: 'wss://127.0.0.1:7777',
                    fingerprint: 'c2a7f31e88d04b6a915c2e7d3fa6b09812ce45d7'
                        'a3f0b16c84d29e5b7a01c6f3',
                    code: 'AB12CD34',
                    expiresInSecs: 120,
                    lanAddresses: () async => ['192.168.1.23', '10.0.0.5'],
                  ),
                ),
                child: const Text('Pair another device'),
              ),
            ),
          ),
        ),
      ),
    ));
    await tester.tap(find.text('Pair another device'));
    await tester.pumpAndSettle();

    expect(find.text('AB12CD34'), findsOneWidget);
    expect(find.text('wss://127.0.0.1:7777'), findsOneWidget);
    expect(find.text('wss://192.168.1.23:7777'), findsOneWidget);
    expect(find.textContaining('--listen 0.0.0.0:7777'), findsOneWidget);
    await _capture(tester, 'pairing_sheet_900x700');

    expect(
      overflowErrors,
      isEmpty,
      reason: 'render overflow in pairing sheet: '
          '${overflowErrors.map((e) => e.exceptionAsString()).join('; ')}',
    );
  });
}
