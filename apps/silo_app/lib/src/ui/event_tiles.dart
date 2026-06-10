/// Transcript tiles for each event payload kind.
library;

import 'dart:convert';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import '../connection/event_store.dart';
import '../protocol/event.dart';
import 'theme.dart';

/// Builds the transcript widget for [event], or null when the event has no
/// transcript representation (cost reports, turn markers, and answers,
/// which render with their question).
Widget? buildEventTile(BuildContext context, Event event, EventStore store) {
  final payload = event.payload;
  return switch (payload) {
    UserPromptPayload() => _UserPromptBubble(payload: payload),
    AssistantTextPayload() => _AssistantTextTile(payload: payload),
    ToolUsePayload() => _ToolUseTile(payload: payload),
    ToolResultPayload() => _ToolResultTile(payload: payload),
    AgentSpawnedPayload() => _AgentMarker(
        agent: payload.agent,
        icon: Icons.call_split,
        text: '${payload.agent} spawned by ${payload.parent}',
        detail: payload.prompt,
      ),
    AgentCompletedPayload() => _AgentMarker(
        agent: payload.agent,
        icon: payload.isError ? Icons.error_outline : Icons.check_circle_outline,
        text:
            '${payload.agent} ${payload.isError ? 'failed' : 'completed'}',
        detail: payload.result,
        isError: payload.isError,
      ),
    QuestionAskedPayload() => _QuestionTile(payload: payload, store: store),
    QuestionAnsweredPayload() => null,
    FileSharedPayload() => _FileTile(payload: payload),
    ErrorPayload() => _ErrorTile(payload: payload),
    HarnessStartedPayload() => _SystemLine(
        icon: Icons.power_settings_new,
        text:
            'Harness ${payload.harnessId} started · ${payload.llm} · ${payload.sandbox}',
      ),
    ShutdownPayload() => _SystemLine(
        icon: Icons.stop_circle_outlined,
        text: payload.message == null
            ? 'Harness shut down'
            : 'Harness shut down: ${payload.message}',
      ),
    AwaitingInputPayload() => null,
    TurnCompletePayload() => null,
    CostReportPayload() => null,
    AccessReportUpdatedPayload() => null,
    UnknownPayload() => _SystemLine(
        icon: Icons.help_outline,
        text: 'Unrecognized event (${payload.kind})',
      ),
  };
}

/// Left padding for output produced by subagents, so nested agent activity
/// reads as indented under the top-level conversation.
double _agentIndent(String agent) => agent == 'agent-0' ? 0 : 24;

String _prettyJson(Object? value) {
  try {
    return const JsonEncoder.withIndent('  ').convert(value);
  } catch (_) {
    return '$value';
  }
}

class _UserPromptBubble extends StatelessWidget {
  const _UserPromptBubble({required this.payload});

  final UserPromptPayload payload;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Align(
      alignment: Alignment.centerRight,
      child: Container(
        margin: const EdgeInsets.fromLTRB(48, 4, 12, 4),
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
        decoration: BoxDecoration(
          color: scheme.primaryContainer,
          borderRadius: const BorderRadius.only(
            topLeft: Radius.circular(16),
            topRight: Radius.circular(16),
            bottomLeft: Radius.circular(16),
            bottomRight: Radius.circular(4),
          ),
        ),
        child: SelectableText(
          payload.text,
          style: TextStyle(color: scheme.onPrimaryContainer),
        ),
      ),
    );
  }
}

class _AgentLabel extends StatelessWidget {
  const _AgentLabel({required this.agent});

  final String agent;

  @override
  Widget build(BuildContext context) {
    if (agent == 'agent-0') {
      return const SizedBox.shrink();
    }
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: 2),
      child: Text(
        agent,
        style: Theme.of(context)
            .textTheme
            .labelSmall
            ?.copyWith(color: scheme.tertiary),
      ),
    );
  }
}

class _AssistantTextTile extends StatelessWidget {
  const _AssistantTextTile({required this.payload});

  final AssistantTextPayload payload;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.fromLTRB(
          12 + _agentIndent(payload.agent), 4, 48, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _AgentLabel(agent: payload.agent),
          SelectableText(payload.text),
        ],
      ),
    );
  }
}

class _CollapsiblePayloadTile extends StatelessWidget {
  const _CollapsiblePayloadTile({
    required this.agent,
    required this.icon,
    required this.title,
    required this.body,
    this.isError = false,
  });

  final String agent;
  final IconData icon;
  final String title;
  final String body;
  final bool isError;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = isError ? scheme.error : scheme.onSurfaceVariant;
    return Padding(
      padding: EdgeInsets.fromLTRB(
          12 + _agentIndent(agent), 2, 48, 2),
      child: Card(
        elevation: 0,
        color: scheme.surfaceContainerHighest.withValues(alpha: 0.6),
        clipBehavior: Clip.antiAlias,
        child: Theme(
          data: Theme.of(context).copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            dense: true,
            leading: Icon(icon, size: 18, color: color),
            title: Text(
              title,
              style: Theme.of(context)
                  .textTheme
                  .bodyMedium
                  ?.copyWith(color: color),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
            expandedCrossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SelectableText(
                body,
                style: const TextStyle(
                  fontFamily: monoFontFamily,
                  fontFamilyFallback: ['Menlo', 'Courier New'],
                  fontSize: 12,
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _ToolUseTile extends StatelessWidget {
  const _ToolUseTile({required this.payload});

  final ToolUsePayload payload;

  @override
  Widget build(BuildContext context) {
    return _CollapsiblePayloadTile(
      agent: payload.agent,
      icon: Icons.build_outlined,
      title: payload.call.name,
      body: _prettyJson(payload.call.input),
    );
  }
}

class _ToolResultTile extends StatelessWidget {
  const _ToolResultTile({required this.payload});

  final ToolResultPayload payload;

  @override
  Widget build(BuildContext context) {
    return _CollapsiblePayloadTile(
      agent: payload.agent,
      icon: payload.output.isError
          ? Icons.error_outline
          : Icons.subdirectory_arrow_right,
      title:
          '${payload.toolName} ${payload.output.isError ? 'failed' : 'result'}',
      body: payload.output.content,
      isError: payload.output.isError,
    );
  }
}

class _AgentMarker extends StatelessWidget {
  const _AgentMarker({
    required this.agent,
    required this.icon,
    required this.text,
    required this.detail,
    this.isError = false,
  });

  final String agent;
  final IconData icon;
  final String text;
  final String detail;
  final bool isError;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = isError ? scheme.error : scheme.tertiary;
    return Padding(
      padding: EdgeInsets.fromLTRB(12 + _agentIndent(agent), 2, 48, 2),
      child: Card(
        elevation: 0,
        color: scheme.tertiaryContainer.withValues(alpha: 0.35),
        clipBehavior: Clip.antiAlias,
        child: Theme(
          data: Theme.of(context).copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            dense: true,
            leading: Icon(icon, size: 18, color: color),
            title: Text(
              text,
              style: Theme.of(context)
                  .textTheme
                  .bodyMedium
                  ?.copyWith(color: color),
            ),
            childrenPadding: const EdgeInsets.fromLTRB(16, 0, 16, 12),
            expandedCrossAxisAlignment: CrossAxisAlignment.start,
            children: [SelectableText(detail)],
          ),
        ),
      ),
    );
  }
}

class _QuestionTile extends StatelessWidget {
  const _QuestionTile({required this.payload, required this.store});

  final QuestionAskedPayload payload;
  final EventStore store;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final answer = store.answerFor(payload.id);
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 48, 4),
      child: Container(
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          border: Border.all(color: scheme.outlineVariant),
          borderRadius: BorderRadius.circular(12),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Icon(Icons.help_outline, size: 16, color: scheme.secondary),
                const SizedBox(width: 6),
                Expanded(
                  child: Text(
                    payload.question.question,
                    style: Theme.of(context).textTheme.bodyMedium,
                  ),
                ),
              ],
            ),
            const SizedBox(height: 6),
            if (answer != null)
              Row(
                children: [
                  Icon(Icons.reply, size: 16, color: scheme.primary),
                  const SizedBox(width: 6),
                  Expanded(
                    child: Text(
                      answer,
                      style: Theme.of(context)
                          .textTheme
                          .bodyMedium
                          ?.copyWith(fontWeight: FontWeight.w600),
                    ),
                  ),
                ],
              )
            else
              Text(
                'Awaiting answer…',
                style: Theme.of(context)
                    .textTheme
                    .bodySmall
                    ?.copyWith(fontStyle: FontStyle.italic),
              ),
          ],
        ),
      ),
    );
  }
}

class _FileTile extends StatelessWidget {
  const _FileTile({required this.payload});

  final FileSharedPayload payload;

  Future<void> _save(BuildContext context) async {
    final messenger = ScaffoldMessenger.of(context);
    try {
      final location = await getSaveLocation(suggestedName: payload.name);
      if (location == null) {
        return;
      }
      final bytes = base64Decode(payload.contentB64);
      await XFile.fromData(bytes, name: payload.name).saveTo(location.path);
      messenger.showSnackBar(
        SnackBar(content: Text('Saved ${payload.name}')),
      );
    } on UnimplementedError {
      messenger.showSnackBar(
        const SnackBar(content: Text('Saving is not supported here')),
      );
    } catch (error) {
      messenger.showSnackBar(SnackBar(content: Text('Save failed: $error')));
    }
  }

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final origin = payload.origin;
    final from = switch (origin) {
      ClientOrigin() => 'uploaded by ${origin.clientId}',
      LlmOrigin() => 'from ${origin.agent}',
    };
    final size = (payload.contentB64.length * 3 / 4).round();
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 48, 4),
      child: Card(
        elevation: 0,
        color: scheme.surfaceContainerHigh,
        child: ListTile(
          leading: const Icon(Icons.insert_drive_file_outlined),
          title: Text(payload.name),
          subtitle: Text('$from · ${_formatSize(size)}'),
          trailing: IconButton(
            icon: const Icon(Icons.save_alt),
            tooltip: 'Save file',
            onPressed: () => _save(context),
          ),
        ),
      ),
    );
  }
}

String _formatSize(int bytes) {
  if (bytes < 1024) {
    return '$bytes B';
  }
  if (bytes < 1024 * 1024) {
    return '${(bytes / 1024).toStringAsFixed(1)} KB';
  }
  return '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MB';
}

class _ErrorTile extends StatelessWidget {
  const _ErrorTile({required this.payload});

  final ErrorPayload payload;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.fromLTRB(12, 4, 48, 4),
      child: Container(
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          color: scheme.errorContainer,
          borderRadius: BorderRadius.circular(12),
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(Icons.error_outline, color: scheme.onErrorContainer),
            const SizedBox(width: 8),
            Expanded(
              child: SelectableText(
                '${payload.context}: ${payload.message}',
                style: TextStyle(color: scheme.onErrorContainer),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _SystemLine extends StatelessWidget {
  const _SystemLine({required this.icon, required this.text});

  final IconData icon;
  final String text;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Icon(icon, size: 14, color: scheme.outline),
          const SizedBox(width: 6),
          Flexible(
            child: Text(
              text,
              style: Theme.of(context)
                  .textTheme
                  .labelSmall
                  ?.copyWith(color: scheme.outline),
              textAlign: TextAlign.center,
            ),
          ),
        ],
      ),
    );
  }
}
