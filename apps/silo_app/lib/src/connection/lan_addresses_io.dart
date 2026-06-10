/// LAN address discovery via `dart:io` network interfaces.
library;

import 'dart:io';

/// Lists this machine's IPv4 addresses that another device on the same
/// network could dial. Loopback and link-local addresses are excluded.
Future<List<String>> listLanIpv4Addresses() async {
  final interfaces = await NetworkInterface.list(
    type: InternetAddressType.IPv4,
    includeLoopback: false,
  );
  final seen = <String>{};
  final addresses = <String>[];
  for (final interface in interfaces) {
    for (final address in interface.addresses) {
      if (address.isLoopback || address.isLinkLocal) {
        continue;
      }
      if (seen.add(address.address)) {
        addresses.add(address.address);
      }
    }
  }
  return addresses;
}
