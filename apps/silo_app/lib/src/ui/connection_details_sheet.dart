/// Bottom sheet listing the raw identifiers behind one harness connection,
/// for troubleshooting, with a switch that turns on raw payload rendering
/// in the transcript for this session.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'theme.dart';

class ConnectionDetailsSheet extends StatelessWidget {
  const ConnectionDetailsSheet({
    super.key,
    required this.harnessId,
    required this.url,
    required this.fingerprint,
    required this.clientId,
    required this.protocolVersion,
    required this.showRawPayloads,
    required this.onShowRawPayloadsChanged,
  });

  final String? harnessId;
  final String url;
  final String? fingerprint;
  final String? clientId;
  final int? protocolVersion;
  final bool showRawPayloads;
  final ValueChanged<bool> onShowRawPayloadsChanged;

  Widget _row(BuildContext context, String label, String? value) {
    final theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  label,
                  style: theme.textTheme.labelSmall
                      ?.copyWith(color: theme.colorScheme.onSurfaceVariant),
                ),
                const SizedBox(height: 2),
                SelectableText(
                  value ?? 'unknown',
                  style: const TextStyle(
                    fontFamily: monoFontFamily,
                    fontFamilyFallback: ['Menlo', 'Courier New'],
                    fontSize: 12.5,
                  ),
                ),
              ],
            ),
          ),
          if (value != null)
            IconButton(
              icon: const Icon(Icons.copy, size: 16),
              tooltip: 'Copy $label',
              onPressed: () {
                Clipboard.setData(ClipboardData(text: value));
                ScaffoldMessenger.maybeOf(context)?.showSnackBar(
                  SnackBar(content: Text('$label copied')),
                );
              },
            ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.fromLTRB(20, 0, 20, 20),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('Connection details',
                style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            _row(context, 'Harness id', harnessId),
            _row(context, 'URL', url),
            _row(context, 'Certificate fingerprint (SHA-256)', fingerprint),
            _row(context, 'This client\'s id', clientId),
            _row(context, 'Protocol version', protocolVersion?.toString()),
            const Divider(height: 24),
            SwitchListTile(
              contentPadding: EdgeInsets.zero,
              value: showRawPayloads,
              onChanged: onShowRawPayloadsChanged,
              title: const Text('Show raw payloads'),
              subtitle: const Text(
                  'Render suppressed tool events and wire identifiers in '
                  'the transcript for this session.'),
            ),
          ],
        ),
      ),
    );
  }
}
