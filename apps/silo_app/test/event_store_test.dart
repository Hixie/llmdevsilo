import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/connection/event_store.dart';
import 'package:silo_app/src/protocol/event.dart';
import 'package:silo_app/src/protocol/types.dart';

Event event(int seq, EventPayload payload) => Event(
      seq: seq,
      time: Timestamp(logical: seq),
      payload: payload,
    );

void main() {
  test('events come out ordered by seq regardless of insert order', () {
    final store = EventStore();
    store.insert(event(2, const AwaitingInputPayload()));
    store.insert(event(0, const UserPromptPayload(text: 'first')));
    store.insert(event(1, const AssistantTextPayload(
      agent: 'agent-0',
      text: 'reply',
    )));
    expect(store.events.map((e) => e.seq).toList(), [0, 1, 2]);
  });

  test('duplicate seq is dropped and does not notify', () {
    final store = EventStore();
    var notifications = 0;
    store.addListener(() => notifications += 1);
    expect(store.insert(event(0, const UserPromptPayload(text: 'a'))), isTrue);
    expect(store.insert(event(0, const UserPromptPayload(text: 'b'))), isFalse);
    expect(store.length, 1);
    expect(
      (store.events.single.payload as UserPromptPayload).text,
      'a',
    );
    expect(notifications, 1);
  });

  test('insertAll skips duplicates and reports how many were added', () {
    final store = EventStore();
    store.insert(event(1, const AwaitingInputPayload()));
    final added = store.insertAll([
      event(0, const UserPromptPayload(text: 'x')),
      event(1, const AwaitingInputPayload()),
      event(2, const AwaitingInputPayload()),
    ]);
    expect(added, 2);
    expect(store.length, 3);
  });

  test('nextSeq is the resume point: one past the highest seq seen', () {
    final store = EventStore();
    expect(store.nextSeq, 0);
    store.insert(event(0, const AwaitingInputPayload()));
    expect(store.nextSeq, 1);
    store.insert(event(7, const AwaitingInputPayload()));
    expect(store.nextSeq, 8);
  });

  test('liveQuestion tracks the unanswered question', () {
    final store = EventStore();
    const question = UserQuestion(question: 'Proceed?');
    store.insert(event(0, const QuestionAskedPayload(
      id: 'q-1',
      agent: 'agent-0',
      question: question,
    )));
    expect(store.liveQuestion?.id, 'q-1');

    store.insert(event(1, const QuestionAnsweredPayload(
      id: 'q-1',
      answer: 'yes',
    )));
    expect(store.liveQuestion, isNull);
    expect(store.answerFor('q-1'), 'yes');

    store.insert(event(2, const QuestionAskedPayload(
      id: 'q-2',
      agent: 'agent-0',
      question: question,
    )));
    expect(store.liveQuestion?.id, 'q-2');
  });

  test('latestCostReports keeps the most recent report per backend', () {
    final store = EventStore();
    store.insert(event(0, const CostReportPayload(
      backend: 'anthropic',
      usage: UsageSnapshot(inputTokens: 1, outputTokens: 1, usd: 0.1),
      quota: QuotaConfig(),
    )));
    store.insert(event(1, const CostReportPayload(
      backend: 'anthropic',
      usage: UsageSnapshot(inputTokens: 5, outputTokens: 5, usd: 0.5),
      quota: QuotaConfig(),
    )));
    store.insert(event(2, const CostReportPayload(
      backend: 'openai',
      usage: UsageSnapshot(inputTokens: 2, outputTokens: 2, usd: 0.2),
      quota: QuotaConfig(),
    )));
    final reports = store.latestCostReports();
    expect(reports.length, 2);
    expect(reports['anthropic']?.usage.usd, 0.5);
    expect(reports['openai']?.usage.usd, 0.2);
  });
}
