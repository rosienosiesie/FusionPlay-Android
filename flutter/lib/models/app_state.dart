enum PlaybackCommand { playPause, previous, next }

enum TrackChangeDirection {
  previous,
  next;

  static TrackChangeDirection? parse(Object? value) {
    final normalized = value?.toString().trim().toLowerCase();
    return switch (normalized) {
      'previous' => TrackChangeDirection.previous,
      'next' => TrackChangeDirection.next,
      _ => null,
    };
  }
}

enum MiPlayDeviceIdentity {
  vehicle('vehicle', 5, '车机'),
  television('television', 2, '电视'),
  tablet('tablet', 18, '平板'),
  speaker('speaker', 4, '音响');

  const MiPlayDeviceIdentity(
    this.persistedValue,
    this.protocolValue,
    this.label,
  );

  final String persistedValue;
  final int protocolValue;
  final String label;

  static MiPlayDeviceIdentity parse(Object? value) {
    final normalized = value?.toString().trim().toLowerCase();
    if (normalized == 'display_speaker') return MiPlayDeviceIdentity.speaker;
    return values.firstWhere(
      (item) => item.persistedValue == normalized,
      orElse: () => MiPlayDeviceIdentity.television,
    );
  }
}

class AppSettings {
  const AppSettings({
    this.schemaVersion = 8,
    this.receiverName,
    this.startupEnabled = true,
    this.autoWakeEnabled = true,
    this.advancedEffectsEnabled = false,
    this.miPlayEnabled = true,
    this.miPlayDeviceIdentity = MiPlayDeviceIdentity.television,
    this.airPlayEnabled = true,
    this.dlnaEnabled = true,
  });

  final int schemaVersion;
  final String? receiverName;
  final bool startupEnabled;
  final bool autoWakeEnabled;
  final bool advancedEffectsEnabled;
  final bool miPlayEnabled;
  final MiPlayDeviceIdentity miPlayDeviceIdentity;
  final bool airPlayEnabled;
  final bool dlnaEnabled;

  factory AppSettings.fromMap(Map<Object?, Object?> map) => AppSettings(
    schemaVersion: _integer(map['schemaVersion']) ?? 8,
    receiverName: _string(map['receiverName']),
    startupEnabled: map['startupEnabled'] as bool? ?? true,
    autoWakeEnabled: map['autoWakeEnabled'] as bool? ?? true,
    advancedEffectsEnabled: map['advancedEffectsEnabled'] as bool? ?? false,
    miPlayEnabled: map['miPlayEnabled'] as bool? ?? true,
    miPlayDeviceIdentity: MiPlayDeviceIdentity.parse(
      map['miPlayDeviceIdentity'],
    ),
    airPlayEnabled: map['airPlayEnabled'] as bool? ?? true,
    dlnaEnabled: map['dlnaEnabled'] as bool? ?? true,
  );

  AppSettings copyWith({
    String? receiverName,
    bool clearReceiverName = false,
    bool? startupEnabled,
    bool? autoWakeEnabled,
    bool? advancedEffectsEnabled,
    bool? miPlayEnabled,
    MiPlayDeviceIdentity? miPlayDeviceIdentity,
    bool? airPlayEnabled,
    bool? dlnaEnabled,
  }) => AppSettings(
    schemaVersion: 8,
    receiverName: clearReceiverName ? null : receiverName ?? this.receiverName,
    startupEnabled: startupEnabled ?? this.startupEnabled,
    autoWakeEnabled: autoWakeEnabled ?? this.autoWakeEnabled,
    advancedEffectsEnabled:
        advancedEffectsEnabled ?? this.advancedEffectsEnabled,
    miPlayEnabled: miPlayEnabled ?? this.miPlayEnabled,
    miPlayDeviceIdentity: miPlayDeviceIdentity ?? this.miPlayDeviceIdentity,
    airPlayEnabled: airPlayEnabled ?? this.airPlayEnabled,
    dlnaEnabled: dlnaEnabled ?? this.dlnaEnabled,
  );

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      other is AppSettings &&
          schemaVersion == other.schemaVersion &&
          receiverName == other.receiverName &&
          startupEnabled == other.startupEnabled &&
          autoWakeEnabled == other.autoWakeEnabled &&
          advancedEffectsEnabled == other.advancedEffectsEnabled &&
          miPlayEnabled == other.miPlayEnabled &&
          miPlayDeviceIdentity == other.miPlayDeviceIdentity &&
          airPlayEnabled == other.airPlayEnabled &&
          dlnaEnabled == other.dlnaEnabled;

  @override
  int get hashCode => Object.hash(
    schemaVersion,
    receiverName,
    startupEnabled,
    autoWakeEnabled,
    advancedEffectsEnabled,
    miPlayEnabled,
    miPlayDeviceIdentity,
    airPlayEnabled,
    dlnaEnabled,
  );
}

class PlaybackSnapshot {
  const PlaybackSnapshot({
    this.title,
    this.artist,
    this.album,
    this.coverArt,
    this.mediaUrl,
    this.mediaKind,
    this.protocol,
    this.qualityText,
    this.durationMs,
    this.positionMs = 0,
    this.volumePercent,
    this.isPlaying = false,
    this.streamActive = false,
    this.sourceEpoch,
    this.trackIdentity,
    this.trackChangeDirection,
  });

  final String? title;
  final String? artist;
  final String? album;
  final String? coverArt;
  final String? mediaUrl;
  final String? mediaKind;
  final String? protocol;
  final String? qualityText;
  final int? durationMs;
  final int positionMs;
  final int? volumePercent;
  final bool isPlaying;
  final bool streamActive;
  final int? sourceEpoch;
  final String? trackIdentity;
  final TrackChangeDirection? trackChangeDirection;

  String? get artworkTransitionIdentity {
    String? normalized(String? value) {
      final result = value?.trim();
      return result == null || result.isEmpty ? null : result;
    }

    final explicitIdentity = normalized(trackIdentity);
    if (explicitIdentity != null) return 'track:$explicitIdentity';

    final urlIdentity = normalized(mediaUrl);
    if (urlIdentity != null) return 'media:$urlIdentity';

    final titleIdentity = normalized(title);
    if (titleIdentity == null) return null;
    final protocolIdentity = normalized(protocol)?.toLowerCase() ?? 'media';
    final artistIdentity = normalized(artist) ?? '';
    return 'metadata:$protocolIdentity\u001f$titleIdentity\u001f$artistIdentity';
  }

  factory PlaybackSnapshot.fromMap(Map<Object?, Object?> map) =>
      PlaybackSnapshot(
        title: _string(map['title']),
        artist: _string(map['artist']),
        album: _string(map['album']),
        coverArt: _string(map['coverArt']),
        mediaUrl: _string(map['mediaUrl']),
        mediaKind: _string(map['mediaKind']),
        protocol: _string(map['protocol']),
        qualityText: _string(map['qualityText']),
        durationMs: _integer(map['durationMs']),
        positionMs: _integer(map['positionMs']) ?? 0,
        volumePercent: _integer(map['volumePercent']),
        isPlaying: map['isPlaying'] as bool? ?? false,
        streamActive: map['streamActive'] as bool? ?? false,
        sourceEpoch: _integer(map['sourceEpoch']),
        trackIdentity: _string(map['trackIdentity']),
        trackChangeDirection: TrackChangeDirection.parse(
          map['trackChangeDirection'],
        ),
      );

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      other is PlaybackSnapshot &&
          title == other.title &&
          artist == other.artist &&
          album == other.album &&
          coverArt == other.coverArt &&
          mediaUrl == other.mediaUrl &&
          mediaKind == other.mediaKind &&
          protocol == other.protocol &&
          qualityText == other.qualityText &&
          durationMs == other.durationMs &&
          positionMs == other.positionMs &&
          volumePercent == other.volumePercent &&
          isPlaying == other.isPlaying &&
          streamActive == other.streamActive &&
          sourceEpoch == other.sourceEpoch &&
          trackIdentity == other.trackIdentity &&
          trackChangeDirection == other.trackChangeDirection;

  @override
  int get hashCode => Object.hashAll([
    title,
    artist,
    album,
    coverArt,
    mediaUrl,
    mediaKind,
    protocol,
    qualityText,
    durationMs,
    positionMs,
    volumePercent,
    isPlaying,
    streamActive,
    sourceEpoch,
    trackIdentity,
    trackChangeDirection,
  ]);
}

class RemoteControlState {
  const RemoteControlState({
    this.available = false,
    this.commands = const {},
    this.transport,
    this.experimental = false,
  });

  final bool available;
  final Set<String> commands;
  final String? transport;
  final bool experimental;

  factory RemoteControlState.fromMap(Map<Object?, Object?> map) =>
      RemoteControlState(
        available: map['available'] as bool? ?? false,
        commands: (map['commands'] as List<Object?>? ?? const [])
            .map((item) => item.toString())
            .toSet(),
        transport: _string(map['transport']),
        experimental: map['experimental'] as bool? ?? false,
      );

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      other is RemoteControlState &&
          available == other.available &&
          commands.length == other.commands.length &&
          commands.containsAll(other.commands) &&
          transport == other.transport &&
          experimental == other.experimental;

  @override
  int get hashCode => Object.hash(
    available,
    Object.hashAllUnordered(commands),
    transport,
    experimental,
  );
}

class AppState {
  const AppState({
    this.initialized = false,
    this.busy = false,
    this.coreRunning = false,
    this.receiverReady = false,
    this.receiverPort,
    this.receiverDeviceId,
    this.connectedClient,
    this.activeMediaSource,
    this.selectedCoreMediaSource,
    this.receiverName = 'FusionPlay',
    this.settings = const AppSettings(),
    this.playback = const PlaybackSnapshot(),
    this.remoteControl = const RemoteControlState(),
    this.lastError,
  });

  final bool initialized;
  final bool busy;
  final bool coreRunning;
  final bool receiverReady;
  final int? receiverPort;
  final String? receiverDeviceId;
  final String? connectedClient;
  final String? activeMediaSource;
  final String? selectedCoreMediaSource;
  final String receiverName;
  final AppSettings settings;
  final PlaybackSnapshot playback;
  final RemoteControlState remoteControl;
  final String? lastError;

  factory AppState.fromMap(Map<Object?, Object?> map) => AppState(
    initialized: map['initialized'] as bool? ?? false,
    busy: map['busy'] as bool? ?? false,
    coreRunning: map['coreRunning'] as bool? ?? false,
    receiverReady: map['receiverReady'] as bool? ?? false,
    receiverPort: _integer(map['receiverPort']),
    receiverDeviceId: _string(map['receiverDeviceId']),
    connectedClient: _string(map['connectedClient']),
    activeMediaSource: _string(map['activeMediaSource']),
    selectedCoreMediaSource: _string(map['selectedCoreMediaSource']),
    receiverName: _string(map['receiverName']) ?? 'FusionPlay',
    settings: AppSettings.fromMap(_map(map['settings'])),
    playback: PlaybackSnapshot.fromMap(_map(map['playback'])),
    remoteControl: RemoteControlState.fromMap(_map(map['remoteControl'])),
    lastError: _string(map['lastError']),
  );

  AppState copyWith({String? lastError}) => AppState(
    initialized: initialized,
    busy: busy,
    coreRunning: coreRunning,
    receiverReady: receiverReady,
    receiverPort: receiverPort,
    receiverDeviceId: receiverDeviceId,
    connectedClient: connectedClient,
    activeMediaSource: activeMediaSource,
    selectedCoreMediaSource: selectedCoreMediaSource,
    receiverName: receiverName,
    settings: settings,
    playback: playback,
    remoteControl: remoteControl,
    lastError: lastError ?? this.lastError,
  );

  @override
  bool operator ==(Object other) =>
      identical(this, other) ||
      other is AppState &&
          initialized == other.initialized &&
          busy == other.busy &&
          coreRunning == other.coreRunning &&
          receiverReady == other.receiverReady &&
          receiverPort == other.receiverPort &&
          receiverDeviceId == other.receiverDeviceId &&
          connectedClient == other.connectedClient &&
          activeMediaSource == other.activeMediaSource &&
          selectedCoreMediaSource == other.selectedCoreMediaSource &&
          receiverName == other.receiverName &&
          settings == other.settings &&
          playback == other.playback &&
          remoteControl == other.remoteControl &&
          lastError == other.lastError;

  @override
  int get hashCode => Object.hashAll([
    initialized,
    busy,
    coreRunning,
    receiverReady,
    receiverPort,
    receiverDeviceId,
    connectedClient,
    activeMediaSource,
    selectedCoreMediaSource,
    receiverName,
    settings,
    playback,
    remoteControl,
    lastError,
  ]);
}

Map<Object?, Object?> _map(Object? value) =>
    value is Map ? value.cast<Object?, Object?>() : const {};

String? _string(Object? value) {
  final text = value?.toString().trim();
  return text == null || text.isEmpty ? null : text;
}

int? _integer(Object? value) =>
    value is num ? value.round() : int.tryParse(value?.toString() ?? '');
