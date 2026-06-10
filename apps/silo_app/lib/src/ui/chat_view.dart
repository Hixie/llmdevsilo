/// The transcript plus the pinned question card. Driven directly by an
/// [EventStore] so it can be exercised in widget tests without a network
/// connection.
library;

import 'package:flutter/material.dart';

import '../connection/event_store.dart';
import 'event_tiles.dart';
import 'question_card.dart';

class ChatView extends StatefulWidget {
  const ChatView({
    super.key,
    required this.store,
    required this.onAnswer,
  });

  final EventStore store;

  /// Called with (questionId, answer) when the user answers the live
  /// question.
  final void Function(String questionId, String answer) onAnswer;

  @override
  State<ChatView> createState() => _ChatViewState();
}

class _ChatViewState extends State<ChatView> {
  final ScrollController _scroll = ScrollController();
  int _lastLength = 0;

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  void _maybeAutoScroll(int length) {
    if (length == _lastLength) {
      return;
    }
    _lastLength = length;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients) {
        _scroll.animateTo(
          _scroll.position.maxScrollExtent,
          duration: const Duration(milliseconds: 200),
          curve: Curves.easeOut,
        );
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: widget.store,
      builder: (context, _) {
        final events = widget.store.events;
        _maybeAutoScroll(events.length);
        final tiles = <Widget>[];
        for (final event in events) {
          final tile = buildEventTile(context, event, widget.store);
          if (tile != null) {
            tiles.add(KeyedSubtree(
              key: ValueKey('event-${event.seq}'),
              child: tile,
            ));
          }
        }
        final live = widget.store.liveQuestion;
        return Column(
          children: [
            Expanded(
              child: tiles.isEmpty
                  ? Center(
                      child: Text(
                        'No activity yet.\nSend a prompt to get started.',
                        textAlign: TextAlign.center,
                        style: Theme.of(context)
                            .textTheme
                            .bodyMedium
                            ?.copyWith(
                              color: Theme.of(context).colorScheme.outline,
                            ),
                      ),
                    )
                  : ListView(
                      controller: _scroll,
                      padding: const EdgeInsets.symmetric(vertical: 8),
                      children: tiles,
                    ),
            ),
            if (live != null)
              QuestionCard(
                key: ValueKey('question-${live.id}'),
                payload: live,
                onAnswer: (answer) => widget.onAnswer(live.id, answer),
              ),
          ],
        );
      },
    );
  }
}
