/// The "Pair another device" sheet: the pairing code with a live
/// countdown, the harness WebSocket URL (with candidate LAN URLs when the
/// connected host is loopback or unspecified), the pinned certificate
/// fingerprint, per-field copy buttons, and a copy-everything action.
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../connection/lan_addresses.dart' as lan;
import 'pairing_info.dart';
import 'theme.dart';

class PairingSheet extends StatefulWidget {
  const PairingSheet({
    super.key,
    required this.url,
    this.fingerprint,
    this.code,
    this.expiresInSecs,
    this.lanAddresses,
  });

  /// The WebSocket URL this app used to reach the harness.
  final String url;

  /// Pinned certificate fingerprint, hex SHA-256.
  final String? fingerprint;

  /// The issued pairing code; null while the request is in flight.
  final String? code;

  /// Validity window reported alongside the code, in seconds.
  final int? expiresInSecs;

  /// LAN address source, replaceable in tests. Defaults to the platform
  /// implementation: network interfaces on desktop and mobile, an empty
  /// list on the web.
  final Future<List<String>> Function()? lanAddresses;

  @override
  State<PairingSheet> createState() => _PairingSheetState();
}

class _PairingSheetState extends State<PairingSheet> {
  Timer? _countdown;
  int? _remaining;
  List<String> _lanUrls = const [];

  Uri get _uri => Uri.parse(widget.url);

  @override
  void initState() {
    super.initState();
    _startCountdown();
    _loadLanUrls();
  }

  @override
  void didUpdateWidget(PairingSheet oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (widget.code != oldWidget.code ||
        widget.expiresInSecs != oldWidget.expiresInSecs) {
      _startCountdown();
    }
  }

  @override
  void dispose() {
    _countdown?.cancel();
    super.dispose();
  }

  /// Tracks the code's validity window with a 1-second tick. The harness
  /// expires the code server-side regardless; this is display only.
  void _startCountdown() {
    _countdown?.cancel();
    _countdown = null;
    final secs = widget.expiresInSecs;
    if (widget.code == null || secs == null) {
      _remaining = null;
      return;
    }
    _remaining = secs;
    _countdown = Timer.periodic(const Duration(seconds: 1), (timer) {
      setState(() {
        final next = (_remaining ?? 0) - 1;
        _remaining = next > 0 ? next : 0;
        if (next <= 0) {
          timer.cancel();
        }
      });
    });
  }

  Future<void> _loadLanUrls() async {
    if (!needsLanCandidates(_uri.host)) {
      return;
    }
    final source = widget.lanAddresses ?? lan.listLanIpv4Addresses;
    final addresses = await source();
    if (!mounted) {
      return;
    }
    setState(() => _lanUrls = lanCandidateUrls(widget.url, addresses));
  }

  Future<void> _copy(String label, String text) async {
    await Clipboard.setData(ClipboardData(text: text));
    if (!mounted) {
      return;
    }
    ScaffoldMessenger.of(context)
        .showSnackBar(SnackBar(content: Text('$label copied')));
  }

  void _copyAll() {
    _copy(
      'Connection details',
      connectionDetailsBlock(
        urls: [widget.url, ..._lanUrls],
        fingerprint: widget.fingerprint,
        code: widget.code,
      ),
    );
  }

  Widget _copyButton(String label, String text) => IconButton(
        icon: const Icon(Icons.copy, size: 18),
        tooltip: 'Copy',
        onPressed: () => _copy(label, text),
      );

  /// A labelled monospace value with a copy button.
  Widget _field(BuildContext context, String label, String value) {
    return Row(
      crossAxisAlignment: CrossAxisAlignment.center,
      children: [
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Text(label, style: Theme.of(context).textTheme.labelSmall),
              const SizedBox(height: 2),
              SelectableText(
                value,
                style:
                    const TextStyle(fontFamily: monoFontFamily, fontSize: 13),
              ),
            ],
          ),
        ),
        _copyButton(label, value),
      ],
    );
  }

  Widget _codeSection(BuildContext context) {
    final code = widget.code;
    final theme = Theme.of(context);
    if (code == null) {
      return const Padding(
        padding: EdgeInsets.symmetric(vertical: 24),
        child: Column(
          children: [
            CircularProgressIndicator(),
            SizedBox(height: 12),
            Text('Requesting a pairing code…'),
          ],
        ),
      );
    }
    final remaining = _remaining;
    final expired = remaining != null && remaining <= 0;
    return Column(
      children: [
        Text('Pairing code', style: theme.textTheme.labelSmall),
        Row(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            SelectableText(
              code,
              style: TextStyle(
                fontFamily: monoFontFamily,
                fontSize: 36,
                fontWeight: FontWeight.w600,
                letterSpacing: 6,
                color: expired
                    ? theme.colorScheme.outline
                    : theme.colorScheme.onSurface,
              ),
            ),
            _copyButton('Pairing code', code),
          ],
        ),
        if (remaining != null)
          Text(
            expired
                ? 'Code expired — close this sheet and request a new one.'
                : 'Expires in $remaining s',
            style: theme.textTheme.bodySmall?.copyWith(
              color: expired
                  ? theme.colorScheme.error
                  : theme.colorScheme.outline,
            ),
          ),
      ],
    );
  }

  /// The warning shown when the harness address is loopback: other devices
  /// cannot reach it, and the harness has to be restarted with a reachable
  /// listen address.
  Widget _loopbackWarning(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final port = _uri.hasPort ? _uri.port : 443;
    return Container(
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: scheme.errorContainer,
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(Icons.warning_amber_rounded,
              size: 18, color: scheme.onErrorContainer),
          const SizedBox(width: 8),
          Expanded(
            child: Text(
              'This harness is listening on the loopback address, so '
              'another device cannot reach it. Restart it with '
              '--listen 0.0.0.0:$port (or a LAN address) and pair again.',
              style: TextStyle(color: scheme.onErrorContainer, fontSize: 13),
            ),
          ),
        ],
      ),
    );
  }

  Widget _lanSection(BuildContext context) {
    final theme = Theme.of(context);
    final intro = isLoopbackHost(_uri.host)
        ? 'If the harness is listening on all interfaces, another device '
            'on your network can try:'
        : 'The harness listens on every interface. From another device on '
            'your network, try:';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text('Addresses on your network', style: theme.textTheme.labelSmall),
        const SizedBox(height: 2),
        Text(intro, style: theme.textTheme.bodySmall),
        for (final url in _lanUrls)
          Row(
            children: [
              Expanded(
                child: SelectableText(
                  url,
                  style: const TextStyle(
                      fontFamily: monoFontFamily, fontSize: 13),
                ),
              ),
              _copyButton('URL', url),
            ],
          ),
      ],
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final host = _uri.host;
    final fingerprint = widget.fingerprint;
    return SafeArea(
      child: SingleChildScrollView(
        padding: EdgeInsets.fromLTRB(
            24, 8, 24, 24 + MediaQuery.of(context).viewInsets.bottom),
        child: Center(
          child: ConstrainedBox(
            constraints: const BoxConstraints(maxWidth: 520),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Text('Pair another device',
                    style: theme.textTheme.titleLarge,
                    textAlign: TextAlign.center),
                const SizedBox(height: 4),
                Text(
                  'On the other device, choose "Pair with a harness" and '
                  'enter these details.',
                  style: theme.textTheme.bodySmall,
                  textAlign: TextAlign.center,
                ),
                const SizedBox(height: 12),
                _codeSection(context),
                const SizedBox(height: 16),
                _field(context, 'WebSocket URL', widget.url),
                if (isLoopbackHost(host)) ...[
                  const SizedBox(height: 8),
                  _loopbackWarning(context),
                ],
                if (_lanUrls.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  _lanSection(context),
                ],
                if (fingerprint != null) ...[
                  const SizedBox(height: 12),
                  _field(context, 'Certificate fingerprint (SHA-256)',
                      fingerprint),
                ],
                const SizedBox(height: 12),
                Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Icon(Icons.public,
                        size: 16, color: theme.colorScheme.outline),
                    const SizedBox(width: 8),
                    Expanded(
                      child: Text(
                        'Browser clients: first open '
                        '${httpsOriginFromWsUrl(widget.url)} in a tab and '
                        'accept the certificate warning — the harness '
                        'serves a confirmation page.',
                        style: theme.textTheme.bodySmall?.copyWith(
                            color: theme.colorScheme.outline),
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 16),
                Row(
                  mainAxisAlignment: MainAxisAlignment.end,
                  children: [
                    TextButton.icon(
                      onPressed: _copyAll,
                      icon: const Icon(Icons.copy_all, size: 18),
                      label: const Text('Copy connection details'),
                    ),
                    const SizedBox(width: 8),
                    FilledButton(
                      onPressed: () => Navigator.of(context).maybePop(),
                      child: const Text('Done'),
                    ),
                  ],
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
