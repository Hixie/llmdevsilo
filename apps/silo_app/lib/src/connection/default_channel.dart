/// Selects the platform WebSocket factory: `dart:io` based with certificate
/// pinning on desktop and mobile, browser WebSocket (no pinning) on web.
library;

export 'default_channel_web.dart' if (dart.library.io) 'default_channel_io.dart';
