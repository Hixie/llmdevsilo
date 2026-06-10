/// The per-harness conversation screen.
library;

import 'package:file_selector/file_selector.dart';
import 'package:flutter/foundation.dart' show kIsWeb;
import 'package:flutter/material.dart';

import '../connection/harness_connection.dart';
import '../protocol/event.dart';
import 'access_sheet.dart';
import 'chat_view.dart';
import 'pairing_info.dart';
import 'pairing_sheet.dart';
import 'theme.dart';

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key, required this.connection});

  final HarnessConnection connection;

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final TextEditingController _input = TextEditingController();
  final FocusNode _inputFocus = FocusNode();

  HarnessConnection get connection => widget.connection;

  @override
  void initState() {
    super.initState();
    connection.connect();
    connection.store.addListener(_onEvents);
    connection.markRead();
  }

  @override
  void dispose() {
    connection.store.removeListener(_onEvents);
    _input.dispose();
    _inputFocus.dispose();
    super.dispose();
  }

  void _onEvents() {
    // The transcript is on screen, so everything that arrives is read.
    connection.markRead();
  }

  void _sendPrompt() {
    final text = _input.text.trim();
    if (text.isEmpty) {
      return;
    }
    connection.sendPrompt(text);
    _input.clear();
    _inputFocus.requestFocus();
  }

  Future<void> _attachFile() async {
    final messenger = ScaffoldMessenger.of(context);
    final file = await openFile();
    if (file == null) {
      return;
    }
    final bytes = await file.readAsBytes();
    connection.uploadFile(file.name, bytes);
    messenger.showSnackBar(
      SnackBar(content: Text('Uploading ${file.name}…')),
    );
  }

  Future<void> _showPairingSheet() async {
    connection.requestPairingCode();
    final fingerprint = await connection.pinnedFingerprint();
    if (!mounted) {
      return;
    }
    await showModalBottomSheet<void>(
      context: context,
      isScrollControlled: true,
      showDragHandle: true,
      builder: (sheetContext) => ListenableBuilder(
        listenable: connection,
        builder: (context, _) {
          final code = connection.issuedPairingCode;
          return PairingSheet(
            url: connection.endpoint.url,
            fingerprint: fingerprint,
            code: code?.code,
            expiresInSecs: code?.expiresInSecs,
          );
        },
      ),
    );
  }

  Future<void> _confirmShutdown() async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('Shut down harness?'),
        content: const Text(
            'This stops the harness for every connected client.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Shut down'),
          ),
        ],
      ),
    );
    if (confirmed == true) {
      connection.requestShutdown();
    }
  }

  Widget _statusChip(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final (color, label) = switch (connection.status) {
      ConnectionStatus.connected => (Colors.green, 'connected'),
      ConnectionStatus.connecting => (Colors.orange, 'connecting'),
      ConnectionStatus.authenticating => (Colors.orange, 'authenticating'),
      ConnectionStatus.reconnecting => (Colors.orange, 'reconnecting'),
      ConnectionStatus.failed => (scheme.error, 'auth failed'),
      ConnectionStatus.disconnected => (scheme.outline, 'offline'),
    };
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(Icons.circle, size: 10, color: color),
        const SizedBox(width: 6),
        Text(label, style: Theme.of(context).textTheme.labelSmall),
      ],
    );
  }

  Widget? _costChip(BuildContext context) {
    final reports = connection.store.latestCostReports();
    double usd = 0;
    int tokens = 0;
    if (reports.isEmpty) {
      for (final entry in connection.costEntries) {
        usd += entry.usage.usd;
        tokens += entry.usage.totalTokens;
      }
      if (connection.costEntries.isEmpty) {
        return null;
      }
    } else {
      for (final report in reports.values) {
        usd += report.usage.usd;
        tokens += report.usage.totalTokens;
      }
    }
    final label =
        '\$${usd.toStringAsFixed(usd < 10 ? 3 : 2)} · ${_formatTokens(tokens)}';
    return Padding(
      padding: const EdgeInsets.only(right: 4),
      child: ActionChip(
        avatar: const Icon(Icons.payments_outlined, size: 16),
        label: Text(label),
        onPressed: () {
          connection.requestCost();
          _showCostDetails(context);
        },
      ),
    );
  }

  void _showCostDetails(BuildContext context) {
    showDialog<void>(
      context: context,
      builder: (context) => ListenableBuilder(
        listenable: connection,
        builder: (context, _) {
          final reports = connection.store.latestCostReports();
          final rows = <Widget>[];
          void addRow(String backend, num usd, int input, int output,
              String? quota) {
            rows.add(ListTile(
              dense: true,
              title: Text(backend),
              subtitle: Text(
                  '$input in / $output out tokens${quota == null ? '' : '\n$quota'}'),
              trailing: Text('\$${usd.toStringAsFixed(4)}'),
            ));
          }

          if (reports.isNotEmpty) {
            for (final report in reports.values) {
              addRow(
                report.backend,
                report.usage.usd,
                report.usage.inputTokens,
                report.usage.outputTokens,
                _quotaLabel(report.quota.maxUsd, report.quota.maxTotalTokens),
              );
            }
          } else {
            for (final entry in connection.costEntries) {
              addRow(
                entry.backend,
                entry.usage.usd,
                entry.usage.inputTokens,
                entry.usage.outputTokens,
                _quotaLabel(entry.quota.maxUsd, entry.quota.maxTotalTokens),
              );
            }
          }
          return AlertDialog(
            title: const Text('Session cost'),
            content: SizedBox(
              width: 360,
              child: rows.isEmpty
                  ? const Text('No cost reports yet.')
                  : Column(mainAxisSize: MainAxisSize.min, children: rows),
            ),
            actions: [
              TextButton(
                onPressed: () => Navigator.of(context).pop(),
                child: const Text('Close'),
              ),
            ],
          );
        },
      ),
    );
  }

  /// The error message followed by certificate guidance for browsers,
  /// which reject the harness's self-signed certificate until the user
  /// accepts it once.
  Widget _withWebCertHelp(BuildContext context, String error) {
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(error),
        const SizedBox(height: 8),
        const Text('If the browser is blocking the harness\'s self-signed '
            'certificate, open this address in a new tab, accept the '
            'certificate warning, then retry:'),
        const SizedBox(height: 4),
        SelectableText(
          httpsOriginFromWsUrl(connection.endpoint.url),
          style: const TextStyle(fontFamily: monoFontFamily, fontSize: 13),
        ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: connection,
      builder: (context, _) {
        final costChip = _costChip(context);
        return Scaffold(
          appBar: AppBar(
            title: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(connection.endpoint.name,
                    style: Theme.of(context).textTheme.titleMedium),
                _statusChip(context),
              ],
            ),
            actions: [
              ?costChip,
              IconButton(
                icon: const Icon(Icons.shield_outlined),
                tooltip: 'Sandbox access',
                onPressed: () {
                  connection.requestAccessReport();
                  showAccessSheet(context, connection.accessReport);
                },
              ),
              PopupMenuButton<String>(
                onSelected: (value) {
                  switch (value) {
                    case 'pair':
                      _showPairingSheet();
                    case 'shutdown':
                      _confirmShutdown();
                  }
                },
                itemBuilder: (context) => const [
                  PopupMenuItem(
                    value: 'pair',
                    child: ListTile(
                      leading: Icon(Icons.qr_code_2),
                      title: Text('Pair another device'),
                    ),
                  ),
                  PopupMenuItem(
                    value: 'shutdown',
                    child: ListTile(
                      leading: Icon(Icons.power_settings_new),
                      title: Text('Shut down harness'),
                    ),
                  ),
                ],
              ),
            ],
          ),
          body: Column(
            children: [
              if (connection.shutdownMessage != null)
                MaterialBanner(
                  content: Text(connection.shutdownMessage!),
                  leading: const Icon(Icons.power_settings_new),
                  actions: const [SizedBox.shrink()],
                ),
              if (connection.status == ConnectionStatus.failed &&
                  connection.lastError != null)
                MaterialBanner(
                  content: kIsWeb
                      ? _withWebCertHelp(context, connection.lastError!)
                      : Text(connection.lastError!),
                  leading: const Icon(Icons.error_outline),
                  actions: [
                    TextButton(
                      onPressed: connection.connect,
                      child: const Text('Retry'),
                    ),
                  ],
                )
              else if (kIsWeb &&
                  connection.lastError != null &&
                  connection.status != ConnectionStatus.connected &&
                  connection.status != ConnectionStatus.authenticating)
                MaterialBanner(
                  content: _withWebCertHelp(
                      context, 'Could not connect: ${connection.lastError!}'),
                  leading: const Icon(Icons.lock_outline),
                  actions: [
                    TextButton(
                      onPressed: connection.connect,
                      child: const Text('Retry'),
                    ),
                  ],
                ),
              Expanded(
                child: ChatView(
                  store: connection.store,
                  onAnswer: connection.answerQuestion,
                ),
              ),
              _inputBar(context),
            ],
          ),
        );
      },
    );
  }

  Widget _inputBar(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final awaiting = connection.store.events.isNotEmpty &&
        connection.store.events.last.payload is AwaitingInputPayload;
    return SafeArea(
      child: Container(
        padding: const EdgeInsets.fromLTRB(8, 6, 8, 8),
        decoration: BoxDecoration(
          color: scheme.surfaceContainerLow,
          border: Border(top: BorderSide(color: scheme.outlineVariant)),
        ),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.end,
          children: [
            IconButton(
              icon: const Icon(Icons.attach_file),
              tooltip: 'Upload file',
              onPressed: _attachFile,
            ),
            Expanded(
              child: TextField(
                controller: _input,
                focusNode: _inputFocus,
                minLines: 1,
                maxLines: 6,
                textInputAction: TextInputAction.send,
                onSubmitted: (_) => _sendPrompt(),
                decoration: InputDecoration(
                  hintText: awaiting
                      ? 'The model is waiting for you…'
                      : 'Message the harness',
                  border: const OutlineInputBorder(
                    borderRadius: BorderRadius.all(Radius.circular(24)),
                  ),
                  isDense: true,
                  contentPadding: const EdgeInsets.symmetric(
                      horizontal: 16, vertical: 10),
                ),
              ),
            ),
            const SizedBox(width: 6),
            IconButton.filled(
              icon: const Icon(Icons.arrow_upward),
              tooltip: 'Send',
              onPressed: _sendPrompt,
            ),
          ],
        ),
      ),
    );
  }
}

String _quotaLabel(double? maxUsd, int? maxTokens) {
  final parts = <String>[
    if (maxUsd != null) 'quota \$${maxUsd.toStringAsFixed(2)}',
    if (maxTokens != null) 'quota ${_formatTokens(maxTokens)}',
  ];
  return parts.isEmpty ? '' : parts.join(' · ');
}

String _formatTokens(int tokens) {
  if (tokens < 1000) {
    return '$tokens tok';
  }
  if (tokens < 1000000) {
    return '${(tokens / 1000).toStringAsFixed(1)}k tok';
  }
  return '${(tokens / 1000000).toStringAsFixed(2)}M tok';
}
