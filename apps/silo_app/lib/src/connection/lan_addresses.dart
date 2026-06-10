/// Selects the platform implementation of LAN address discovery: network
/// interfaces on desktop/mobile (`dart:io`), stubbed out on the web.
library;

export 'lan_addresses_stub.dart'
    if (dart.library.io) 'lan_addresses_io.dart';
