/// Ordered store of harness events, keyed by sequence number.
library;

import 'dart:collection';

import 'package:flutter/foundation.dart';

import '../protocol/event.dart';

/// Holds the events received for one harness, ordered by `seq` and
/// de-duplicated (a backlog response and the live stream can overlap).
class EventStore extends ChangeNotifier {
  final SplayTreeMap<int, Event> _events = SplayTreeMap();

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
        added += 1;
      }
    }
    if (added > 0) {
      notifyListeners();
    }
    return added;
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
    notifyListeners();
  }
}
