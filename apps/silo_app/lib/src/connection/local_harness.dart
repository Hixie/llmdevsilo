/// Selects the platform implementation of local harness discovery and
/// spawning: real on desktop/mobile (`dart:io`), stubbed out on the web.
library;

export 'local_harness_stub.dart'
    if (dart.library.io) 'local_harness_io.dart';
