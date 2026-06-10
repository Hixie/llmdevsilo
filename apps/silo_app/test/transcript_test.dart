// Smoke test: a transcript with one of each major event kind renders.
// Plus rendering rules: pretty agent and client names, the shared left
// gutter, and tool tiles suppressed or revealed by the raw-payload switch.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/event_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';
import 'package:silo_app/src/ui/chat_view.dart';
import 'package:silo_app/src/ui/theme.dart';

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

    expect(find.textContaining('Harness started'), findsOneWidget);
    expect(find.textContaining('work'), findsOneWidget);
    expect(find.text('Please fix the tests'), findsOneWidget);
    expect(find.text('Looking at the failures now.'), findsOneWidget);
    expect(find.text('Bash'), findsOneWidget);
    expect(find.text('Bash result'), findsOneWidget);
    expect(find.text('subagent 1 spawned'), findsOneWidget);
    expect(find.text('Subagent reporting in.'), findsOneWidget);
    // The label over the subagent's output uses the pretty name.
    expect(find.text('subagent 1'), findsOneWidget);
    expect(find.text('subagent 1 completed'), findsOneWidget);
    expect(find.text('diff.patch'), findsOneWidget);
    expect(find.textContaining('rate limited'), findsOneWidget);
    expect(find.textContaining('all done'), findsOneWidget);
    // Raw identifiers never surface by default: no harness id, no raw
    // agent ids, no tool_use ids.
    expect(find.textContaining('h-1'), findsNothing);
    expect(find.textContaining('agent-1'), findsNothing);
    expect(find.textContaining('t-1'), findsNothing);

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

  test('agentDisplayName prefers the spawn name, then the agent ordinal', () {
    final store = EventStore();
    store.insertAll([
      event(0, const AgentSpawnedPayload(
        parent: 'agent-0',
        agent: 'agent-1',
        name: 'refactor tests',
        prompt: 'p',
      )),
      event(1, const AgentSpawnedPayload(
        parent: 'agent-0',
        agent: 'agent-2',
        prompt: 'p',
      )),
      event(2, const AgentSpawnedPayload(
        parent: 'agent-0',
        agent: 'feed',
        prompt: 'p',
      )),
    ]);
    expect(store.agentDisplayName('agent-0'), 'main agent');
    expect(store.agentDisplayName('agent-1'), 'refactor tests');
    expect(store.agentDisplayName('agent-2'), 'subagent 2');
    // An id without a trailing number falls back to the spawn position.
    expect(store.agentDisplayName('feed'), 'subagent 3');
    // An agent that never spawned still gets its ordinal from the id.
    expect(store.agentDisplayName('agent-7'), 'subagent 7');
  });

  testWidgets('prompts from other clients are labeled with the client name',
      (tester) async {
    final store = EventStore();
    store.insertAll([
      event(0, const UserPromptPayload(
        clientId: 'c-self',
        clientName: 'My laptop',
        text: 'mine',
      )),
      event(1, const UserPromptPayload(
        clientId: 'c-other',
        clientName: "Ian's phone",
        text: 'theirs',
      )),
      event(2, const UserPromptPayload(text: 'anonymous')),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ChatView(
          store: store,
          onAnswer: (_, _) {},
          selfClientId: 'c-self',
        ),
      ),
    ));
    await tester.pump();

    expect(find.text("Ian's phone"), findsOneWidget);
    // This client's own prompts carry no label.
    expect(find.text('My laptop'), findsNothing);
    expect(find.text('mine'), findsOneWidget);
    expect(find.text('anonymous'), findsOneWidget);
  });

  testWidgets('subagent tiles show the model-given name', (tester) async {
    final store = EventStore();
    store.insertAll([
      event(0, const AgentSpawnedPayload(
        parent: 'agent-0',
        agent: 'agent-1',
        name: 'refactor tests',
        prompt: 'go refactor',
      )),
      event(1, const AssistantTextPayload(
        agent: 'agent-1',
        text: 'Subagent reporting in.',
      )),
      event(2, const AgentCompletedPayload(
        agent: 'agent-1',
        result: 'done',
        isError: false,
      )),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ChatView(store: store, onAnswer: (_, _) {}),
      ),
    ));
    await tester.pump();

    expect(find.text('refactor tests spawned'), findsOneWidget);
    expect(find.text('refactor tests'), findsOneWidget);
    expect(find.text('refactor tests completed'), findsOneWidget);
    expect(find.textContaining('agent-1'), findsNothing);
  });

  testWidgets(
      'AskUserQuestion and SendUserFile tool events render only with raw '
      'payloads on', (tester) async {
    final store = EventStore();
    store.insertAll([
      event(0, const ToolUsePayload(
        agent: 'agent-0',
        call: ToolCall(
          id: 't-q',
          name: 'AskUserQuestion',
          input: {'question': 'Which color?'},
        ),
      )),
      event(1, const ToolResultPayload(
        agent: 'agent-0',
        toolUseId: 't-q',
        toolName: 'AskUserQuestion',
        output: ToolOutput(content: 'Red', isError: false),
      )),
      event(2, const ToolUsePayload(
        agent: 'agent-0',
        call: ToolCall(
          id: 't-f',
          name: 'SendUserFile',
          input: {'path': '/tmp/diff.patch'},
        ),
      )),
      event(3, const ToolUsePayload(
        agent: 'agent-0',
        call: ToolCall(
          id: 't-b',
          name: 'Bash',
          input: {'command': 'cargo test'},
        ),
      )),
    ]);

    Widget app({required bool showRaw}) => MaterialApp(
          home: Scaffold(
            body: ChatView(
              store: store,
              onAnswer: (_, _) {},
              showRawPayloads: showRaw,
            ),
          ),
        );

    await tester.pumpWidget(app(showRaw: false));
    await tester.pump();
    // Question and file tool events are carried by other transcript
    // elements, so their raw tiles are hidden; tool ids never show.
    expect(find.textContaining('AskUserQuestion'), findsNothing);
    expect(find.textContaining('SendUserFile'), findsNothing);
    expect(find.text('Bash'), findsOneWidget);
    expect(find.textContaining('t-b'), findsNothing);

    await tester.pumpWidget(app(showRaw: true));
    await tester.pump();
    expect(find.text('AskUserQuestion · t-q'), findsOneWidget);
    expect(find.text('AskUserQuestion result · t-q'), findsOneWidget);
    expect(find.text('SendUserFile · t-f'), findsOneWidget);
    expect(find.text('Bash · t-b'), findsOneWidget);
  });

  testWidgets('prompts, assistant text, and tool tiles share one left gutter',
      (tester) async {
    final store = EventStore();
    store.insertAll([
      event(0, const UserPromptPayload(clientId: 'c-1', text: 'a prompt')),
      event(1, const AssistantTextPayload(
        agent: 'agent-0',
        text: 'some answer',
      )),
      event(2, const ToolUsePayload(
        agent: 'agent-0',
        call: ToolCall(id: 't-1', name: 'Bash', input: {'command': 'ls'}),
      )),
    ]);

    await tester.pumpWidget(MaterialApp(
      home: Scaffold(
        body: ChatView(store: store, onAnswer: (_, _) {}),
      ),
    ));
    await tester.pump();

    final bubbleLeft = tester
        .getTopLeft(find
            .ancestor(
                of: find.text('a prompt'),
                matching: find.byType(DecoratedBox))
            .first)
        .dx;
    final textLeft = tester.getTopLeft(find.text('some answer')).dx;
    final cardLeft = tester
        .getTopLeft(find
            .ancestor(of: find.text('Bash'), matching: find.byType(Material))
            .first)
        .dx;
    expect(bubbleLeft, contentGutter);
    expect(textLeft, contentGutter);
    expect(cardLeft, contentGutter);
  });
}
