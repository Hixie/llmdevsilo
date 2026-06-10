// Smoke test: a transcript with one of each major event kind renders.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/event_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';
import 'package:silo_app/src/ui/chat_view.dart';

Event event(int seq, EventPayload payload) =>
    Event(seq: seq, time: Timestamp(logical: seq), payload: payload);

void main() {
  testWidgets('transcript renders all major event kinds', (tester) async {
    tester.view.physicalSize = const Size(800, 2400);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    final store = EventStore();
    store.insertAll([
      event(0, const HarnessStartedPayload(
        harnessId: 'h-1',
        workspace: '/work',
        sandbox: 'mock',
        llm: 'anthropic',
      )),
      event(1, const UserPromptPayload(
        clientId: 'c-1',
        text: 'Please fix the tests',
      )),
      event(2, const AssistantTextPayload(
        agent: 'agent-0',
        text: 'Looking at the failures now.',
      )),
      event(3, const ToolUsePayload(
        agent: 'agent-0',
        call: ToolCall(
          id: 't-1',
          name: 'Bash',
          input: {'command': 'cargo test'},
        ),
      )),
      event(4, const ToolResultPayload(
        agent: 'agent-0',
        toolUseId: 't-1',
        toolName: 'Bash',
        output: ToolOutput(content: '3 tests failed', isError: false),
      )),
      event(5, const AgentSpawnedPayload(
        parent: 'agent-0',
        agent: 'agent-1',
        prompt: 'investigate flaky test',
      )),
      event(6, const AssistantTextPayload(
        agent: 'agent-1',
        text: 'Subagent reporting in.',
      )),
      event(7, const AgentCompletedPayload(
        agent: 'agent-1',
        result: 'found the race',
        isError: false,
      )),
      event(8, const FileSharedPayload(
        name: 'diff.patch',
        contentB64: 'aGVsbG8=',
        origin: LlmOrigin(agent: 'agent-0'),
      )),
      event(9, const ErrorPayload(
        context: 'llm',
        message: 'rate limited',
      )),
      event(10, const TurnCompletePayload(
        agent: 'agent-0',
        stopReason: StopReason.endTurn,
      )),
      event(11, const AwaitingInputPayload()),
      event(12, const ShutdownPayload(message: 'all done')),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ChatView(store: store, onAnswer: (_, _) {}),
      ),
    ));
    await tester.pump();

    expect(find.textContaining('Harness h-1 started'), findsOneWidget);
    expect(find.text('Please fix the tests'), findsOneWidget);
    expect(find.text('Looking at the failures now.'), findsOneWidget);
    expect(find.text('Bash'), findsOneWidget);
    expect(find.text('Bash result'), findsOneWidget);
    expect(find.text('agent-1 spawned by agent-0'), findsOneWidget);
    expect(find.text('Subagent reporting in.'), findsOneWidget);
    expect(find.text('agent-1 completed'), findsOneWidget);
    expect(find.text('diff.patch'), findsOneWidget);
    expect(find.textContaining('rate limited'), findsOneWidget);
    expect(find.textContaining('all done'), findsOneWidget);

    // Tool payloads are collapsed until expanded.
    expect(find.textContaining('cargo test'), findsNothing);
    await tester.tap(find.text('Bash'));
    await tester.pumpAndSettle();
    expect(find.textContaining('cargo test'), findsOneWidget);
  });

  testWidgets('transcript follows new events only when already near the bottom',
      (tester) async {
    tester.view.physicalSize = const Size(800, 400);
    tester.view.devicePixelRatio = 1.0;
    addTearDown(tester.view.reset);

    final store = EventStore();
    store.insertAll([
      for (var i = 0; i < 30; i++)
        event(i, AssistantTextPayload(agent: 'agent-0', text: 'line $i')),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ChatView(store: store, onAnswer: (_, _) {}),
      ),
    ));
    await tester.pump();

    // SelectableText embeds its own Scrollable, so pick the transcript list's.
    final position = tester
        .state<ScrollableState>(find
            .descendant(
                of: find.byType(ListView), matching: find.byType(Scrollable))
            .first)
        .position;
    expect(position.pixels, position.maxScrollExtent);

    // At the bottom: a new event keeps the view pinned there.
    store.insertAll(
        [event(30, const AssistantTextPayload(agent: 'agent-0', text: 'new'))]);
    await tester.pump();
    await tester.pump();
    expect(position.pixels, position.maxScrollExtent);

    // Scrolled up to read history: a new event leaves the view alone.
    position.jumpTo(0);
    await tester.pump();
    store.insertAll([
      event(31, const AssistantTextPayload(agent: 'agent-0', text: 'newer')),
    ]);
    await tester.pump();
    await tester.pump();
    expect(position.pixels, 0);

    // Back near the bottom: following resumes.
    position.jumpTo(position.maxScrollExtent - 10);
    await tester.pump();
    store.insertAll([
      event(32, const AssistantTextPayload(agent: 'agent-0', text: 'newest')),
    ]);
    await tester.pump();
    await tester.pump();
    expect(position.pixels, position.maxScrollExtent);
  });
}
