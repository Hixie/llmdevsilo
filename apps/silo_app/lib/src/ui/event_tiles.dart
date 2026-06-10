/// Transcript tiles for each event payload kind.
library;

import 'dart:convert';

import 'package:file_selector/file_selector.dart';
import 'package:flutter/material.dart';

import '../connection/endpoint.dart';
import '../connection/event_store.dart';
import '../protocol/event.dart';
import 'theme.dart';

/// Tools whose events are carried by other transcript elements (the
/// question card and the file tile), so their raw tool tiles render only
/// when raw payloads are on.
const Set<String> _uiToolNames = {'AskUserQuestion', 'SendUserFile'};

/// Builds the transcript widget for [event], or null when the event has no
/// transcript representation (cost reports, turn markers, answers — which
/// render with their question — and, unless [showRaw] is set, the tool
/// events backing the question card and file sharing).
///
/// [selfClientId] is this client's id, used to label prompts and uploads
/// from other clients. [showRaw] turns on debug rendering: suppressed tool
/// tiles appear and tool tiles show their wire ids.
Widget? buildEventTile(
  BuildContext context,
  Event event,
  EventStore store, {
  String? selfClientId,
  bool showRaw = false,
}) {
  final payload = event.payload;
  return switch (payload) {
    UserPromptPayload() =>
      _UserPromptBubble(payload: payload, selfClientId: selfClientId),
    AssistantTextPayload() => _AssistantTextTile(payload: payload, store: store),
    ToolUsePayload() => !showRaw && _uiToolNames.contains(payload.call.name)
        ? null
        : _ToolUseTile(payload: payload, showRaw: showRaw),
    ToolResultPayload() => !showRaw && _uiToolNames.contains(payload.toolName)
        ? null
        : _ToolResultTile(payload: payload, showRaw: showRaw),
    AgentSpawnedPayload() => _AgentMarker(
        agent: payload.agent,
        icon: Icons.call_split,
        text: payload.parent == 'agent-0'
            ? '${store.agentDisplayName(payload.agent)} spawned'
            : '${store.agentDisplayName(payload.agent)} spawned by '
                '${store.agentDisplayName(payload.parent)}',
        detail: payload.prompt,
      ),
    AgentCompletedPayload() => _AgentMarker(
        agent: payload.agent,
        icon: payload.isError ? Icons.error_outline : Icons.check_circle_outline,
        text: '${store.agentDisplayName(payload.agent)} '
            '${payload.isError ? 'failed' : 'completed'}',
        detail: payload.result,
        isError: payload.isError,
      ),
    QuestionAskedPayload() => _QuestionTile(payload: payload, store: store),
    QuestionAnsweredPayload() => null,
    FileSharedPayload() => _FileTile(
        payload: payload,
        store: store,
        selfClientId: selfClientId,
        showRaw: showRaw,
      ),
    ErrorPayload() => _ErrorTile(payload: payload),
    HarnessStartedPayload() => _SystemLine(
        icon: Icons.power_settings_new,
        text: 'Harness started · '
            '${workspaceFolderName(payload.workspace) ?? payload.workspace}'
            ' · ${payload.llm} · ${payload.sandbox}',
      ),
    ShutdownPayload() => _SystemLine(
        icon: Icons.stop_circle_outlined,
        text: payload.message == null
            ? 'Harness shut down'
            : 'Harness shut down: ${payload.message}',
      ),
    InterruptedPayload() => const _InterruptedTile(),
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

/// Extra left padding for output produced by subagents, so nested agent
/// activity reads as indented under the top-level conversation.
double _agentIndent(String agent) => agent == 'agent-0' ? 0 : 24;

String _prettyJson(Object? value) {
  try {
    return const JsonEncoder.withIndent('  ').convert(value);
  } catch (_) {
    return '$value';
  }
}

class _UserPromptBubble extends StatelessWidget {
  const _UserPromptBubble({required this.payload, this.selfClientId});

  final UserPromptPayload payload;
  final String? selfClientId;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final clientName = payload.clientName;
    final fromOtherClient = payload.clientId != selfClientId;
    return Align(
      alignment: Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.fromLTRB(contentGutter, 4, 48, 4),
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
        decoration: BoxDecoration(
          color: scheme.primaryContainer,
          borderRadius: const BorderRadius.only(
            topLeft: Radius.circular(4),
            topRight: Radius.circular(16),
            bottomLeft: Radius.circular(16),
            bottomRight: Radius.circular(16),
          ),
        ),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            if (clientName != null && fromOtherClient)
              Padding(
                padding: const EdgeInsets.only(bottom: 2),
                child: Text(
                  clientName,
                  style: Theme.of(context).textTheme.labelSmall?.copyWith(
                        color: scheme.onPrimaryContainer
                            .withValues(alpha: 0.7),
                      ),
                ),
              ),
            SelectableText(
              payload.text,
              style: TextStyle(color: scheme.onPrimaryContainer),
            ),
          ],
        ),
      ),
    );
  }
}

class _AgentLabel extends StatelessWidget {
  const _AgentLabel({required this.agent, required this.store});

  final String agent;
  final EventStore store;

  @override
  Widget build(BuildContext context) {
    if (agent == 'agent-0') {
      return const SizedBox.shrink();
    }
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: 2),
      child: Text(
        store.agentDisplayName(agent),
        style: Theme.of(context)
            .textTheme
            .labelSmall
            ?.copyWith(color: scheme.tertiary),
      ),
    );
  }
}

class _AssistantTextTile extends StatelessWidget {
  const _AssistantTextTile({required this.payload, required this.store});

  final AssistantTextPayload payload;
  final EventStore store;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: EdgeInsets.fromLTRB(
          contentGutter + _agentIndent(payload.agent), 4, 48, 4),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          _AgentLabel(agent: payload.agent, store: store),
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
    this.tint,
  });

  final String agent;
  final IconData icon;
  final String title;
  final String body;
  final bool isError;

  /// Surface and text tint; defaults to the demoted tool-tile treatment.
  final Color? tint;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = isError ? scheme.error : (tint ?? scheme.onSurfaceVariant);
    return Padding(
      padding: EdgeInsets.fromLTRB(
          contentGutter + _agentIndent(agent), 3, 48, 3),
      child: Card(
        elevation: 0,
        margin: EdgeInsets.zero,
        color: tint == null
            ? scheme.surfaceContainerLow
            : scheme.tertiaryContainer.withValues(alpha: 0.35),
        clipBehavior: Clip.antiAlias,
        child: Theme(
          data: Theme.of(context).copyWith(dividerColor: Colors.transparent),
          child: ExpansionTile(
            dense: true,
            visualDensity: VisualDensity.compact,
            tilePadding: const EdgeInsets.symmetric(horizontal: 10),
            leading: Icon(icon, size: 16, color: color),
            title: Text(
              title,
              style: Theme.of(context)
                  .textTheme
                  .bodySmall
                  ?.copyWith(color: color),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            childrenPadding: const EdgeInsets.fromLTRB(12, 0, 12, 10),
            expandedCrossAxisAlignment: CrossAxisAlignment.start,
            expandedAlignment: Alignment.topLeft,
            children: [
              SelectableText(
                body,
                textAlign: TextAlign.left,
                style: const TextStyle(
                  fontFamily: monoFontFamily,
                  fontFamilyFallback: ['Menlo', 'Courier New'],
                  fontSize: 11.5,
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
  const _ToolUseTile({required this.payload, this.showRaw = false});

  final ToolUsePayload payload;
  final bool showRaw;

  @override
  Widget build(BuildContext context) {
    return _CollapsiblePayloadTile(
      agent: payload.agent,
      icon: Icons.build_outlined,
      title: showRaw
          ? '${payload.call.name} · ${payload.call.id}'
          : payload.call.name,
      body: _prettyJson(payload.call.input),
    );
  }
}

class _ToolResultTile extends StatelessWidget {
  const _ToolResultTile({required this.payload, this.showRaw = false});

  final ToolResultPayload payload;
  final bool showRaw;

  @override
  Widget build(BuildContext context) {
    final title =
        '${payload.toolName} ${payload.output.isError ? 'failed' : 'result'}';
    return _CollapsiblePayloadTile(
      agent: payload.agent,
      icon: payload.output.isError
          ? Icons.error_outline
          : Icons.subdirectory_arrow_right,
      title: showRaw ? '$title · ${payload.toolUseId}' : title,
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
    return _CollapsiblePayloadTile(
      agent: agent,
      icon: icon,
      title: text,
      body: detail,
      isError: isError,
      tint: scheme.tertiary,
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
      padding: const EdgeInsets.fromLTRB(contentGutter, 4, 48, 4),
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
              crossAxisAlignment: CrossAxisAlignment.start,
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
                crossAxisAlignment: CrossAxisAlignment.start,
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
  const _FileTile({
    required this.payload,
    required this.store,
    this.selfClientId,
    this.showRaw = false,
  });

  final FileSharedPayload payload;
  final EventStore store;
  final String? selfClientId;
  final bool showRaw;

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
      ClientOrigin() => showRaw
          ? 'uploaded by ${origin.clientId}'
          : origin.clientId == selfClientId
              ? 'uploaded by this device'
              : 'uploaded by another device',
      LlmOrigin() => 'from ${store.agentDisplayName(origin.agent)}',
    };
    final size = (payload.contentB64.length * 3 / 4).round();
    return Padding(
      padding: const EdgeInsets.fromLTRB(contentGutter, 4, 48, 4),
      child: Card(
        elevation: 0,
        margin: EdgeInsets.zero,
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
      padding: const EdgeInsets.fromLTRB(contentGutter, 4, 48, 4),
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

class _InterruptedTile extends StatelessWidget {
  const _InterruptedTile();

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.symmetric(
          horizontal: contentGutter, vertical: 6),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.center,
        children: [
          Flexible(
            child: Container(
              padding:
                  const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
              decoration: BoxDecoration(
                color: scheme.errorContainer.withValues(alpha: 0.5),
                borderRadius: BorderRadius.circular(16),
              ),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(Icons.stop_circle_outlined,
                      size: 16, color: scheme.error),
                  const SizedBox(width: 6),
                  Flexible(
                    child: Text(
                      'interrupted by the user',
                      style: Theme.of(context)
                          .textTheme
                          .labelMedium
                          ?.copyWith(color: scheme.error),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ],
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
      padding: const EdgeInsets.symmetric(
          horizontal: contentGutter, vertical: 8),
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
