/// The pinned card shown while a `question_asked` event is awaiting an
/// answer.
library;

import 'package:flutter/material.dart';

import '../protocol/event.dart';
import 'theme.dart';

/// Renders a live [UserQuestion]. Single-select options answer immediately
/// when tapped; multi-select shows checkbox tiles and a submit button; an
/// optional free-text field submits its content as the answer (appended to
/// the selection in multi-select mode).
///
/// The card body scrolls internally, so the card respects whatever maximum
/// height its parent imposes.
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
  final ScrollController _scroll = ScrollController();

  @override
  void dispose() {
    _freeText.dispose();
    _scroll.dispose();
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

  /// Submits the free-text field: joined with the selection in multi-select
  /// mode, on its own otherwise.
  void _submitText() {
    if (_question.multiSelect) {
      _submitMulti();
    } else {
      _submitFreeText();
    }
  }

  /// Header row: the circled question-mark icon, top-aligned with the
  /// first line of the question text.
  Widget _header(ThemeData theme) {
    final scheme = theme.colorScheme;
    return Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Container(
          width: 28,
          height: 28,
          decoration: BoxDecoration(
            color: scheme.primaryContainer,
            shape: BoxShape.circle,
          ),
          child: Icon(
            Icons.question_mark_rounded,
            size: 16,
            color: scheme.onPrimaryContainer,
          ),
        ),
        const SizedBox(width: 10),
        Expanded(
          child: Text(
            _question.question,
            style: theme.textTheme.titleMedium,
          ),
        ),
      ],
    );
  }

  Widget _singleSelectOption(ThemeData theme, QuestionOption option) {
    final scheme = theme.colorScheme;
    return SizedBox(
      width: double.infinity,
      child: FilledButton.tonal(
        style: FilledButton.styleFrom(
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(12),
          ),
          padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 13),
          alignment: Alignment.centerLeft,
        ),
        onPressed: () => widget.onAnswer(option.label),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(option.label),
            if (option.description.isNotEmpty) ...[
              const SizedBox(height: 2),
              Text(
                option.description,
                style: theme.textTheme.bodySmall
                    ?.copyWith(color: scheme.onSurfaceVariant),
              ),
            ],
          ],
        ),
      ),
    );
  }

  Widget _multiSelectOption(ThemeData theme, QuestionOption option) {
    final scheme = theme.colorScheme;
    return CheckboxListTile(
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
      controlAffinity: ListTileControlAffinity.leading,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
      tileColor: scheme.surfaceContainerHighest,
      dense: true,
      contentPadding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
      title: Text(option.label, style: theme.textTheme.bodyMedium),
      subtitle: option.description.isEmpty
          ? null
          : Text(
              option.description,
              style: theme.textTheme.bodySmall
                  ?.copyWith(color: scheme.onSurfaceVariant),
            ),
    );
  }

  Widget _freeTextSection(ThemeData theme) {
    final scheme = theme.colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        if (_question.options.isNotEmpty) ...[
          const SizedBox(height: 14),
          Row(
            children: [
              const Expanded(child: Divider()),
              Padding(
                padding: const EdgeInsets.symmetric(horizontal: 10),
                child: Text(
                  'Or answer in your own words',
                  style: theme.textTheme.labelSmall
                      ?.copyWith(color: scheme.onSurfaceVariant),
                ),
              ),
              const Expanded(child: Divider()),
            ],
          ),
          const SizedBox(height: 10),
        ] else
          const SizedBox(height: 12),
        TextField(
          controller: _freeText,
          textInputAction: TextInputAction.send,
          onSubmitted: (_) => _submitText(),
          decoration: InputDecoration(
            hintText: 'Type your answer…',
            isDense: true,
            border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(12),
            ),
            suffixIcon: IconButton(
              icon: const Icon(Icons.send, size: 18),
              tooltip: 'Send answer',
              onPressed: _submitText,
            ),
          ),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final scheme = theme.colorScheme;
    final question = _question;
    return Padding(
      padding: const EdgeInsets.fromLTRB(contentGutter, 4, contentGutter, 8),
      child: Material(
        color: scheme.surfaceContainerHigh,
        elevation: 1,
        surfaceTintColor: Colors.transparent,
        clipBehavior: Clip.antiAlias,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(16),
          side: BorderSide(color: scheme.outlineVariant),
        ),
        child: Scrollbar(
          controller: _scroll,
          child: SingleChildScrollView(
            controller: _scroll,
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              mainAxisSize: MainAxisSize.min,
              children: [
                _header(theme),
                if (question.options.isNotEmpty) const SizedBox(height: 12),
                if (question.multiSelect) ...[
                  for (final option in question.options) ...[
                    _multiSelectOption(theme, option),
                    const SizedBox(height: 6),
                  ],
                ] else ...[
                  for (final option in question.options) ...[
                    _singleSelectOption(theme, option),
                    const SizedBox(height: 8),
                  ],
                ],
                if (question.allowFreeText) _freeTextSection(theme),
                if (question.multiSelect) ...[
                  const SizedBox(height: 12),
                  FilledButton.icon(
                    onPressed: _submitMulti,
                    icon: const Icon(Icons.send, size: 18),
                    label: const Text('Answer'),
                  ),
                ],
              ],
            ),
          ),
        ),
      ),
    );
  }
}
