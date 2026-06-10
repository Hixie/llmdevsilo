// Tests for the pure helpers behind the pairing sheet: host
// classification, LAN candidate URL construction, the wss-to-https origin
// derivation, and the copyable details block.

import 'package:flutter_test/flutter_test.dart';
import 'package:silo_app/src/ui/pairing_info.dart';

void main() {
  group('host classification', () {
    test('loopback hosts', () {
      expect(isLoopbackHost('127.0.0.1'), isTrue);
      expect(isLoopbackHost('127.1.2.3'), isTrue);
      expect(isLoopbackHost('localhost'), isTrue);
      expect(isLoopbackHost('LOCALHOST'), isTrue);
      expect(isLoopbackHost('::1'), isTrue);
      expect(isLoopbackHost('192.168.1.5'), isFalse);
      expect(isLoopbackHost('example.com'), isFalse);
      expect(isLoopbackHost('0.0.0.0'), isFalse);
    });

    test('unspecified hosts', () {
      expect(isUnspecifiedHost('0.0.0.0'), isTrue);
      expect(isUnspecifiedHost('::'), isTrue);
      expect(isUnspecifiedHost('127.0.0.1'), isFalse);
      expect(isUnspecifiedHost('192.168.1.5'), isFalse);
    });

    test('needsLanCandidates covers loopback and unspecified', () {
      expect(needsLanCandidates('127.0.0.1'), isTrue);
      expect(needsLanCandidates('0.0.0.0'), isTrue);
      expect(needsLanCandidates('192.168.1.5'), isFalse);
    });
  });

  test('lanCandidateUrls keeps scheme and port', () {
    expect(
      lanCandidateUrls('wss://127.0.0.1:7777', ['192.168.1.5', '10.0.0.3']),
      ['wss://192.168.1.5:7777', 'wss://10.0.0.3:7777'],
    );
    expect(lanCandidateUrls('wss://0.0.0.0:1234', []), isEmpty);
  });

  test('httpsOriginFromWsUrl derives the browser origin', () {
    expect(httpsOriginFromWsUrl('wss://192.168.1.9:7777'),
        'https://192.168.1.9:7777/');
    expect(httpsOriginFromWsUrl('wss://example.com'), 'https://example.com/');
    expect(httpsOriginFromWsUrl('ws://127.0.0.1:8080'),
        'http://127.0.0.1:8080/');
    expect(httpsOriginFromWsUrl('wss://[::1]:7777'), 'https://[::1]:7777/');
    expect(httpsOriginFromWsUrl('wss://host:7777/some/path'),
        'https://host:7777/');
  });

  group('connectionDetailsBlock', () {
    test('single URL', () {
      expect(
        connectionDetailsBlock(
          urls: ['wss://192.168.1.9:7777'],
          fingerprint: 'aa11',
          code: 'AB12CD34',
        ),
        'WebSocket URL: wss://192.168.1.9:7777\n'
        'Certificate fingerprint (SHA-256): aa11\n'
        'Pairing code: AB12CD34\n',
      );
    });

    test('several URLs, missing fingerprint and code', () {
      expect(
        connectionDetailsBlock(
          urls: ['wss://127.0.0.1:7777', 'wss://192.168.1.5:7777'],
        ),
        'WebSocket URLs:\n'
        '  wss://127.0.0.1:7777\n'
        '  wss://192.168.1.5:7777\n',
      );
    });
  });
}
