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
    this.selfClientId,
    this.showRawPayloads = false,
  });

  final EventStore store;

  /// Called with (questionId, answer) when the user answers the live
  /// question.
  final void Function(String questionId, String answer) onAnswer;

  /// This client's id, so prompts and uploads from other clients can be
  /// labeled. Null while unknown.
  final String? selfClientId;

  /// Debug rendering: suppressed tool tiles appear and tool tiles show
  /// their wire ids.
  final bool showRawPayloads;

  @override
  State<ChatView> createState() => _ChatViewState();
}

class _ChatViewState extends State<ChatView> {
  final ScrollController _scroll = ScrollController();
  int _lastLength = 0;

  /// Whether the view tracks new content. True while the user is within
  /// [_followSlop] of the bottom; scrolling further up releases the view so
  /// new events do not pull it back down.
  bool _followBottom = true;

  static const double _followSlop = 64;

  @override
  void initState() {
    super.initState();
    _scroll.addListener(_onScroll);
  }

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  void _onScroll() {
    final position = _scroll.position;
    _followBottom = position.pixels >= position.maxScrollExtent - _followSlop;
  }

  void _maybeAutoScroll(int length) {
    if (length == _lastLength) {
      return;
    }
    _lastLength = length;
    if (!_followBottom) {
      return;
    }
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && _scroll.hasClients) {
        _scroll.jumpTo(_scroll.position.maxScrollExtent);
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
          final tile = buildEventTile(
            context,
            event,
            widget.store,
            selfClientId: widget.selfClientId,
            showRaw: widget.showRawPayloads,
          );
          if (tile != null) {
            tiles.add(KeyedSubtree(
              key: ValueKey('event-${event.seq}'),
              child: tile,
            ));
          }
        }
        final live = widget.store.liveQuestion;
        return LayoutBuilder(
          builder: (context, constraints) {
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
                  ConstrainedBox(
                    // Cap the card; its body scrolls internally beyond this.
                    constraints: BoxConstraints(
                      maxHeight: constraints.maxHeight * 0.45,
                    ),
                    child: QuestionCard(
                      key: ValueKey('question-${live.id}'),
                      payload: live,
                      onAnswer: (answer) => widget.onAnswer(live.id, answer),
                    ),
                  ),
              ],
            );
          },
        );
      },
    );
  }
}
