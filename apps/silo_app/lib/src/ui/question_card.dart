/// The pinned card shown while a `question_asked` event is awaiting an
/// answer.
library;

import 'package:flutter/material.dart';

import '../protocol/event.dart';

/// Renders a live [UserQuestion]. Single-select options answer immediately
/// when tapped; multi-select shows checkboxes and a submit button; an
/// optional free-text field submits its content as the answer (appended to
/// the selection in multi-select mode).
class QuestionCard extends StatefulWidget {
  const QuestionCard({
    super.key,
    required this.payload,
    required this.onAnswer,
  });

  final QuestionAskedPayload payload;

  /// Called with the chosen answer text.
  final ValueChanged<String> onAnswer;

  @override
  State<QuestionCard> createState() => _QuestionCardState();
}

class _QuestionCardState extends State<QuestionCard> {
  final Set<String> _selected = {};
  final TextEditingController _freeText = TextEditingController();

  @override
  void dispose() {
    _freeText.dispose();
    super.dispose();
  }

  UserQuestion get _question => widget.payload.question;

  void _submitMulti() {
    final parts = [
      for (final option in _question.options)
        if (_selected.contains(option.label)) option.label,
      if (_question.allowFreeText && _freeText.text.trim().isNotEmpty)
        _freeText.text.trim(),
    ];
    if (parts.isEmpty) {
      return;
    }
    widget.onAnswer(parts.join(', '));
  }

  void _submitFreeText() {
    final text = _freeText.text.trim();
    if (text.isEmpty) {
      return;
    }
    widget.onAnswer(text);
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final question = _question;
    return Card(
      margin: const EdgeInsets.fromLTRB(12, 4, 12, 8),
      color: theme.colorScheme.secondaryContainer,
      child: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            Row(
              children: [
                Icon(Icons.help_outline,
                    size: 20, color: theme.colorScheme.onSecondaryContainer),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    question.question,
                    style: theme.textTheme.titleMedium?.copyWith(
                      color: theme.colorScheme.onSecondaryContainer,
                    ),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 12),
            if (question.multiSelect) ...[
              for (final option in question.options)
                CheckboxListTile(
                  dense: true,
                  contentPadding: EdgeInsets.zero,
                  controlAffinity: ListTileControlAffinity.leading,
                  title: Text(option.label),
                  subtitle: option.description.isEmpty
                      ? null
                      : Text(option.description),
                  value: _selected.contains(option.label),
                  onChanged: (checked) {
                    setState(() {
                      if (checked == true) {
                        _selected.add(option.label);
                      } else {
                        _selected.remove(option.label);
                      }
                    });
                  },
                ),
            ] else ...[
              Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  for (final option in question.options)
                    Tooltip(
                      message: option.description,
                      child: FilledButton.tonal(
                        onPressed: () => widget.onAnswer(option.label),
                        child: Text(option.label),
                      ),
                    ),
                ],
              ),
            ],
            if (question.allowFreeText) ...[
              const SizedBox(height: 8),
              TextField(
                controller: _freeText,
                decoration: const InputDecoration(
                  hintText: 'Other answer…',
                  border: OutlineInputBorder(),
                  isDense: true,
                ),
                onSubmitted: question.multiSelect
                    ? null
                    : (_) => _submitFreeText(),
              ),
            ],
            if (question.multiSelect ||
                (question.allowFreeText && !question.multiSelect)) ...[
              const SizedBox(height: 12),
              Align(
                alignment: Alignment.centerRight,
                child: FilledButton.icon(
                  onPressed:
                      question.multiSelect ? _submitMulti : _submitFreeText,
                  icon: const Icon(Icons.send, size: 18),
                  label: const Text('Answer'),
                ),
              ),
            ],
          ],
        ),
      ),
    );
  }
}
