// Widget tests for the pinned question card: single select, multi-select,
// free text, answer dispatch, and dismissal when question_answered arrives.

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/event_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';
import 'package:silo_app/src/ui/chat_view.dart';
import 'package:silo_app/src/ui/question_card.dart';

Widget wrap(Widget child) => MaterialApp(home: Scaffold(body: child));

Event event(int seq, EventPayload payload) =>
    Event(seq: seq, time: Timestamp(logical: seq), payload: payload);

QuestionAskedPayload asked(
  String id, {
  List<QuestionOption> options = const [],
  bool multiSelect = false,
  bool allowFreeText = false,
}) =>
    QuestionAskedPayload(
      id: id,
      agent: 'agent-0',
      question: UserQuestion(
        question: 'Pick something',
        options: options,
        multiSelect: multiSelect,
        allowFreeText: allowFreeText,
      ),
    );

void main() {
  testWidgets('the circled icon aligns with the top of the question text',
      (tester) async {
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked('q-1', options: const [
        QuestionOption(label: 'Yes'),
        QuestionOption(label: 'No'),
      ]),
      onAnswer: (_) {},
    )));

    final circle = find
        .ancestor(
          of: find.byIcon(Icons.question_mark_rounded),
          matching: find.byType(Container),
        )
        .first;
    expect(
      tester.getTopLeft(circle).dy,
      tester.getTopLeft(find.text('Pick something')).dy,
    );
  });

  testWidgets('single select answers immediately on tap', (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked('q-1', options: const [
        QuestionOption(label: 'Red', description: 'warm'),
        QuestionOption(label: 'Blue'),
      ]),
      onAnswer: (value) => answer = value,
    )));

    // Option descriptions render under the label.
    expect(find.text('warm'), findsOneWidget);

    await tester.tap(find.text('Blue'));
    await tester.pump();
    expect(answer, 'Blue');
  });

  testWidgets('multi-select joins the checked options on submit',
      (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked(
        'q-1',
        options: const [
          QuestionOption(label: 'Red'),
          QuestionOption(label: 'Green'),
          QuestionOption(label: 'Blue'),
        ],
        multiSelect: true,
      ),
      onAnswer: (value) => answer = value,
    )));

    await tester.tap(find.text('Red'));
    await tester.tap(find.text('Blue'));
    await tester.pump();
    await tester.tap(find.text('Answer'));
    await tester.pump();
    expect(answer, 'Red, Blue');
  });

  testWidgets('multi-select submit does nothing with no selection',
      (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked(
        'q-1',
        options: const [QuestionOption(label: 'Red')],
        multiSelect: true,
      ),
      onAnswer: (value) => answer = value,
    )));

    await tester.tap(find.text('Answer'));
    await tester.pump();
    expect(answer, isNull);
  });

  testWidgets('free text submits the typed answer', (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked('q-1', allowFreeText: true),
      onAnswer: (value) => answer = value,
    )));

    await tester.enterText(find.byType(TextField), '  custom answer  ');
    await tester.tap(find.byTooltip('Send answer'));
    await tester.pump();
    expect(answer, 'custom answer');
  });

  testWidgets('free text submits on enter', (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked('q-1', allowFreeText: true),
      onAnswer: (value) => answer = value,
    )));

    await tester.enterText(find.byType(TextField), 'typed answer');
    await tester.testTextInput.receiveAction(TextInputAction.send);
    await tester.pump();
    expect(answer, 'typed answer');
  });

  testWidgets('multi-select appends free text to the selection',
      (tester) async {
    String? answer;
    await tester.pumpWidget(wrap(QuestionCard(
      payload: asked(
        'q-1',
        options: const [QuestionOption(label: 'Red')],
        multiSelect: true,
        allowFreeText: true,
      ),
      onAnswer: (value) => answer = value,
    )));

    await tester.tap(find.text('Red'));
    await tester.enterText(find.byType(TextField), 'and purple');
    await tester.tap(find.text('Answer'));
    await tester.pump();
    expect(answer, 'Red, and purple');
  });

  testWidgets(
      'card appears with question_asked, dispatches the answer, and is '
      'dismissed by question_answered', (tester) async {
    final store = EventStore();
    final answers = <(String, String)>[];
    await tester.pumpWidget(wrap(ChatView(
      store: store,
      onAnswer: (id, answer) => answers.add((id, answer)),
    )));

    expect(find.byType(QuestionCard), findsNothing);

    store.insert(event(0, asked('q-1', options: const [
      QuestionOption(label: 'Yes'),
      QuestionOption(label: 'No'),
    ])));
    await tester.pump();
    expect(find.byType(QuestionCard), findsOneWidget);

    await tester.tap(find.text('Yes'));
    await tester.pump();
    expect(answers, [('q-1', 'Yes')]);

    // The card stays until the server confirms (first answer wins across
    // all clients).
    expect(find.byType(QuestionCard), findsOneWidget);

    store.insert(event(1, const QuestionAnsweredPayload(
      id: 'q-1',
      clientId: 'someone-else',
      answer: 'No',
    )));
    await tester.pump();
    expect(find.byType(QuestionCard), findsNothing);

    // The resolved question and answer render inline in the transcript.
    expect(find.text('Pick something'), findsOneWidget);
    expect(find.text('No'), findsOneWidget);
    await tester.pumpAndSettle();
  });
}
