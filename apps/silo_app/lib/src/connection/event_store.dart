/// Ordered store of harness events, keyed by sequence number.
library;

import 'dart:collection';

import 'package:flutter/foundation.dart';

import '../protocol/event.dart';

/// Holds the events received for one harness, ordered by `seq` and
/// de-duplicated (a backlog response and the live stream can overlap).
class EventStore extends ChangeNotifier {
  final SplayTreeMap<int, Event> _events = SplayTreeMap();

  /// Sequence number of the highest event so far that affects the busy
  /// flag, with the flag value it set. -1 while no such event has arrived.
  int _busySeq = -1;
  bool _busy = false;

  /// Whether the harness is working on something. Activity events
  /// (prompts, assistant output, tool calls and results, agent and
  /// question events) set the flag; `awaiting_input`, `interrupted`, and
  /// `shutdown` clear it. The highest-sequence classified event wins, so
  /// out-of-order backlog inserts resolve correctly. An empty store is
  /// idle.
  bool get isBusy => _busy;

  /// True when [payload] marks the harness busy, false when it marks it
  /// idle, null when it does not affect the busy flag.
  static bool? busySignal(EventPayload payload) => switch (payload) {
        UserPromptPayload() ||
        AssistantTextPayload() ||
        ToolUsePayload() ||
        ToolResultPayload() ||
        AgentSpawnedPayload() ||
        AgentCompletedPayload() ||
        QuestionAskedPayload() ||
        QuestionAnsweredPayload() =>
          true,
        AwaitingInputPayload() ||
        InterruptedPayload() ||
        ShutdownPayload() =>
          false,
        _ => null,
      };

  void _trackBusy(Event event) {
    final signal = busySignal(event.payload);
    if (signal != null && event.seq > _busySeq) {
      _busySeq = event.seq;
      _busy = signal;
    }
  }

  /// Events in sequence order.
  List<Event> get events => List.unmodifiable(_events.values);

  int get length => _events.length;

  bool get isEmpty => _events.isEmpty;

  /// The sequence number to request next when (re)connecting: one past the
  /// highest sequence number seen, or zero for an empty store.
  int get nextSeq => _events.isEmpty ? 0 : _events.lastKey()! + 1;

  Event? bySeq(int seq) => _events[seq];

  /// Inserts one event. Returns false (and does not notify) if an event with
  /// the same sequence number is already present.
  bool insert(Event event) {
    if (_events.containsKey(event.seq)) {
      return false;
    }
    _events[event.seq] = event;
    _trackBusy(event);
    notifyListeners();
    return true;
  }

  /// Inserts a batch, skipping duplicates. Returns the number of events
  /// added; notifies once if any were.
  int insertAll(Iterable<Event> batch) {
    var added = 0;
    for (final event in batch) {
      if (!_events.containsKey(event.seq)) {
        _events[event.seq] = event;
        _trackBusy(event);
        added += 1;
      }
    }
    if (added > 0) {
      notifyListeners();
    }
    return added;
  }

  /// Display name for [agent]. The top-level agent is "main agent".
  /// Subagents use the name given at spawn when one is present, else
  /// "subagent N" with N taken from the trailing number of the agent id,
  /// falling back to the agent's spawn position when the id has none.
  String agentDisplayName(String agent) {
    if (agent == 'agent-0') {
      return 'main agent';
    }
    var position = 0;
    int? spawnPosition;
    for (final event in _events.values) {
      final payload = event.payload;
      if (payload is AgentSpawnedPayload) {
        position += 1;
        if (payload.agent == agent) {
          final name = payload.name?.trim() ?? '';
          if (name.isNotEmpty) {
            return name;
          }
          spawnPosition = position;
          break;
        }
      }
    }
    final match = RegExp(r'(\d+)$').firstMatch(agent);
    if (match != null) {
      return 'subagent ${match.group(1)}';
    }
    return spawnPosition == null ? 'subagent' : 'subagent $spawnPosition';
  }

  /// The workspace path from the latest `harness_started` event, or null
  /// when none has arrived.
  String? get latestWorkspace {
    String? workspace;
    for (final event in _events.values) {
      final payload = event.payload;
      if (payload is HarnessStartedPayload) {
        workspace = payload.workspace;
      }
    }
    return workspace;
  }

  /// The most recent `question_asked` whose id has no matching
  /// `question_answered`, or null when no question is awaiting an answer.
  QuestionAskedPayload? get liveQuestion {
    final answered = <String>{};
    QuestionAskedPayload? live;
    for (final event in _events.values) {
      final payload = event.payload;
      if (payload is QuestionAnsweredPayload) {
        answered.add(payload.id);
      } else if (payload is QuestionAskedPayload) {
        live = payload;
      }
    }
    if (live != null && !answered.contains(live.id)) {
      return live;
    }
    return null;
  }

  /// The answer recorded for question [id], or null while it is unanswered.
  String? answerFor(String id) {
    for (final event in _events.values) {
      final payload = event.payload;
      if (payload is QuestionAnsweredPayload && payload.id == id) {
        return payload.answer;
      }
    }
    return null;
  }

  /// Latest `cost_report` payload per backend name.
  Map<String, CostReportPayload> latestCostReports() {
    final result = <String, CostReportPayload>{};
    for (final event in _events.values) {
      final payload = event.payload;
      if (payload is CostReportPayload) {
        result[payload.backend] = payload;
      }
    }
    return result;
  }

  void clear() {
    _events.clear();
    _busySeq = -1;
    _busy = false;
    notifyListeners();
  }
}
