import 'dart:async';
import 'dart:ui';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'backend/native_app_controller.dart';
import 'models/app_state.dart';
import 'theme/fusion_theme.dart';
import 'ui/artwork.dart';
import 'ui/player_view.dart';
import 'ui/remote_focus.dart';
import 'ui/settings_view.dart';
import 'ui/vector_icon.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final imageCache = PaintingBinding.instance.imageCache;
  imageCache.maximumSize = 8;
  imageCache.maximumSizeBytes = 32 * 1024 * 1024;
  await SystemChrome.setPreferredOrientations(const [
    DeviceOrientation.landscapeLeft,
    DeviceOrientation.landscapeRight,
  ]);
  await SystemChrome.setEnabledSystemUIMode(SystemUiMode.immersiveSticky);
  SystemChrome.setSystemUIOverlayStyle(
    const SystemUiOverlayStyle(
      statusBarColor: Colors.transparent,
      systemNavigationBarColor: Colors.transparent,
      systemNavigationBarDividerColor: Colors.transparent,
      statusBarIconBrightness: Brightness.dark,
      systemNavigationBarIconBrightness: Brightness.dark,
      systemNavigationBarContrastEnforced: false,
    ),
  );
  runApp(const FusionPlayApplication());
}

class FusionPlayApplication extends StatefulWidget {
  const FusionPlayApplication({super.key});

  @override
  State<FusionPlayApplication> createState() => _FusionPlayApplicationState();
}

class _FusionPlayApplicationState extends State<FusionPlayApplication> {
  final NativeAppController controller = NativeAppController();

  @override
  void initState() {
    super.initState();
    controller.initialize();
  }

  @override
  void dispose() {
    controller.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => MaterialApp(
    debugShowCheckedModeBanner: false,
    title: 'FusionPlay',
    theme: fusionTheme(),
    builder: (context, child) =>
        FusionTypographyScope(child: child ?? const SizedBox.shrink()),
    home: ListenableBuilder(
      listenable: controller,
      builder: (context, _) =>
          FusionPlayHome(controller: controller, state: controller.state),
    ),
  );
}

class FusionPlayHome extends StatefulWidget {
  const FusionPlayHome({
    super.key,
    required this.controller,
    required this.state,
  });

  final NativeAppController controller;
  final AppState state;

  @override
  State<FusionPlayHome> createState() => _FusionPlayHomeState();
}

class _FusionPlayHomeState extends State<FusionPlayHome> {
  static const _navigationKeyChannel = MethodChannel(
    'com.fusionplay.android/navigation_keys',
  );

  bool settingsOpen = false;
  final FocusNode _settingsButtonFocus = FocusNode(
    debugLabel: 'settings-button',
  );
  final FocusNode _playerProgressFocus = FocusNode(
    debugLabel: 'player-progress',
  );
  final FocusNode _settingsInitialFocus = FocusNode(
    debugLabel: 'settings-miplay',
  );
  String? _displayedArtworkSource;
  String? _requestedArtworkSource;
  String? _displayedArtworkIdentity;
  String? _requestedArtworkIdentity;
  bool _artworkPreparationStarted = false;
  Timer? _artworkClearTimer;

  @override
  void initState() {
    super.initState();
    _requestedArtworkSource = _normalizedArtworkSource(
      widget.state.playback.coverArt,
    );
    _requestedArtworkIdentity = widget.state.playback.artworkTransitionIdentity;
    _navigationKeyChannel.setMethodCallHandler((call) async {
      if (call.method == 'menu' && mounted) _handleMenuShortcut();
    });
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    if (_artworkPreparationStarted) return;
    _artworkPreparationStarted = true;
    unawaited(
      _prepareArtwork(_requestedArtworkSource, _requestedArtworkIdentity),
    );
  }

  @override
  void didUpdateWidget(covariant FusionPlayHome oldWidget) {
    super.didUpdateWidget(oldWidget);
    final nextArtwork = _normalizedArtworkSource(
      widget.state.playback.coverArt,
    );
    final nextIdentity = widget.state.playback.artworkTransitionIdentity;
    if (nextArtwork == _requestedArtworkSource &&
        nextIdentity == _requestedArtworkIdentity) {
      return;
    }
    _requestedArtworkSource = nextArtwork;
    _requestedArtworkIdentity = nextIdentity;
    unawaited(_prepareArtwork(nextArtwork, nextIdentity));
  }

  @override
  void dispose() {
    _navigationKeyChannel.setMethodCallHandler(null);
    _artworkClearTimer?.cancel();
    _settingsButtonFocus.dispose();
    _playerProgressFocus.dispose();
    _settingsInitialFocus.dispose();
    super.dispose();
  }

  Future<void> _prepareArtwork(String? source, String? identity) async {
    _artworkClearTimer?.cancel();
    if (source == null) {
      if (_displayedArtworkSource == null) return;
      _artworkClearTimer = Timer(artworkClearGracePeriod, () {
        if (mounted &&
            _requestedArtworkSource == source &&
            _requestedArtworkIdentity == identity) {
          setState(() {
            _displayedArtworkSource = null;
            _displayedArtworkIdentity = identity;
          });
        }
      });
      return;
    }
    final provider = artworkProvider(source);
    if (provider == null) return;
    try {
      await precacheImage(provider, context);
      if (mounted &&
          _requestedArtworkSource == source &&
          _requestedArtworkIdentity == identity) {
        setState(() {
          _displayedArtworkSource = source;
          _displayedArtworkIdentity = identity;
        });
      }
    } catch (_) {
      // Preserve the decoded previous cover through transient load failures.
    }
  }

  String? _normalizedArtworkSource(String? source) {
    final value = source?.trim();
    return value == null || value.isEmpty ? null : value;
  }

  void _openSettings() {
    setState(() => settingsOpen = true);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _settingsInitialFocus.requestFocus();
    });
  }

  void _closeSettings() {
    setState(() => settingsOpen = false);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _settingsButtonFocus.requestFocus();
    });
  }

  void _handleBackShortcut() {
    if (settingsOpen) _closeSettings();
  }

  void _handleMenuShortcut() {
    if (settingsOpen) {
      _closeSettings();
    } else {
      _openSettings();
    }
  }

  @override
  Widget build(BuildContext context) {
    final mediaQuery = MediaQuery.of(context);
    final landscape = mediaQuery.orientation == Orientation.landscape;
    final state = widget.state;
    final advancedEffectsEnabled =
        state.settings.advancedEffectsEnabled && !mediaQuery.disableAnimations;
    final artwork = _displayedArtworkSource;
    final hasArtwork = artwork != null && artwork.isNotEmpty;
    final playback = state.playback;
    final hasPlayback =
        playback.streamActive ||
        (playback.protocol?.trim().isNotEmpty ?? false) ||
        (playback.mediaUrl?.trim().isNotEmpty ?? false) ||
        (playback.title?.trim().isNotEmpty ?? false) ||
        (playback.artist?.trim().isNotEmpty ?? false) ||
        playback.durationMs != null;
    final settingsWidth = landscape
        ? (MediaQuery.sizeOf(context).height * 9 / 16).clamp(360.0, 480.0)
        : MediaQuery.sizeOf(context).width;
    final playerSurface = Stack(
      children: [
        Positioned.fill(
          child: ArtworkBackground(source: artwork, hasPlayback: hasPlayback),
        ),
        SafeArea(
          child: Stack(
            children: [
              PlayerView(
                state: state,
                artworkSource: artwork,
                artworkIdentity: _displayedArtworkIdentity,
                progressFocusNode: _playerProgressFocus,
                settingsFocusNode: _settingsButtonFocus,
                onPlayback: widget.controller.playback,
                onSeek: widget.controller.seek,
                onVolume: widget.controller.volume,
              ),
              Positioned(
                right: 14,
                top: 14,
                child: _SettingsButton(
                  light: hasArtwork,
                  focusNode: _settingsButtonFocus,
                  downFocusNode: _playerProgressFocus,
                  onPressed: _openSettings,
                ),
              ),
            ],
          ),
        ),
      ],
    );

    final content = CallbackShortcuts(
      bindings: <ShortcutActivator, VoidCallback>{
        const SingleActivator(LogicalKeyboardKey.escape): _handleBackShortcut,
        const SingleActivator(LogicalKeyboardKey.browserBack):
            _handleBackShortcut,
        const SingleActivator(LogicalKeyboardKey.goBack): _handleBackShortcut,
        const SingleActivator(LogicalKeyboardKey.contextMenu):
            _handleMenuShortcut,
      },
      child: PopScope(
        canPop: !settingsOpen,
        onPopInvokedWithResult: (didPop, _) {
          if (!didPop && settingsOpen) _closeSettings();
        },
        child: Scaffold(
          backgroundColor: hasArtwork
              ? const Color(0xFF202020)
              : FusionColors.playerFallback,
          body: Stack(
            children: [
              Positioned.fill(
                child: ExcludeFocus(
                  excluding: settingsOpen,
                  child: landscape && advancedEffectsEnabled
                      ? TweenAnimationBuilder<double>(
                          tween: Tween(end: settingsOpen ? 1 : 0),
                          duration: settingsOpen
                              ? SettingsStyle.blurEnter
                              : SettingsStyle.blurExit,
                          curve: const Cubic(.23, 1, .32, 1),
                          child: playerSurface,
                          builder: (context, progress, child) {
                            if (progress <= 0.001) return child!;
                            final sigma = SettingsStyle.blurRadius * progress;
                            return ClipRect(
                              child: ImageFiltered(
                                imageFilter: ImageFilter.blur(
                                  sigmaX: sigma,
                                  sigmaY: sigma,
                                  tileMode: TileMode.mirror,
                                ),
                                child: child,
                              ),
                            );
                          },
                        )
                      : playerSurface,
                ),
              ),
              if (landscape)
                Positioned.fill(
                  right: settingsWidth,
                  child: IgnorePointer(
                    ignoring: !settingsOpen,
                    child: AnimatedOpacity(
                      opacity: settingsOpen ? 1 : 0,
                      duration: advancedEffectsEnabled
                          ? (settingsOpen
                                ? SettingsStyle.blurEnter
                                : SettingsStyle.blurExit)
                          : Duration.zero,
                      curve: const Cubic(.23, 1, .32, 1),
                      child: GestureDetector(
                        onTap: _closeSettings,
                        child: ColoredBox(
                          color: Colors.black.withValues(
                            alpha: SettingsStyle.dimAlpha,
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
              AnimatedPositioned(
                duration: advancedEffectsEnabled
                    ? (settingsOpen
                          ? const Duration(milliseconds: 220)
                          : const Duration(milliseconds: 160))
                    : Duration.zero,
                curve: const Cubic(.23, 1, .32, 1),
                top: 0,
                bottom: 0,
                right: settingsOpen ? 0 : -settingsWidth,
                width: settingsWidth,
                child: ExcludeFocus(
                  excluding: !settingsOpen,
                  child: SettingsView(
                    state: state,
                    initialFocusNode: _settingsInitialFocus,
                    onClose: _closeSettings,
                    onReceiverName: widget.controller.setReceiverName,
                    onStartup: widget.controller.setStartupEnabled,
                    onAutoWake: widget.controller.setAutoWakeEnabled,
                    onAdvancedEffects:
                        widget.controller.setAdvancedEffectsEnabled,
                    onMiPlay: (value) =>
                        widget.controller.setProtocolEnabled('miplay', value),
                    onMiPlayIdentity: widget.controller.setMiPlayDeviceIdentity,
                    onAirPlay: (value) =>
                        widget.controller.setProtocolEnabled('airplay', value),
                    onDlna: (value) =>
                        widget.controller.setProtocolEnabled('dlna', value),
                    onExportLogs: widget.controller.exportLogs,
                  ),
                ),
              ),
              if (state.lastError != null)
                SafeArea(
                  child: Align(
                    alignment: Alignment.topCenter,
                    child: Padding(
                      padding: const EdgeInsets.all(16),
                      child: Material(
                        color: const Color(0xFFE3E3E3),
                        borderRadius: BorderRadius.circular(22),
                        child: Padding(
                          padding: const EdgeInsets.only(
                            left: 18,
                            right: 6,
                            top: 6,
                            bottom: 6,
                          ),
                          child: Row(
                            mainAxisSize: MainAxisSize.min,
                            children: [
                              Flexible(
                                child: Text(
                                  state.lastError!,
                                  maxLines: 2,
                                  overflow: TextOverflow.ellipsis,
                                ),
                              ),
                              RemoteFocusFrame(
                                onActivate: widget.controller.clearError,
                                shape: const CircleBorder(),
                                child: IconButton(
                                  onPressed: widget.controller.clearError,
                                  icon: const FusionVectorIcon(
                                    FusionIcons.close,
                                  ),
                                ),
                              ),
                            ],
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
            ],
          ),
        ),
      ),
    );
    return MediaQuery(
      data: mediaQuery.copyWith(
        disableAnimations:
            mediaQuery.disableAnimations ||
            !state.settings.advancedEffectsEnabled,
      ),
      child: content,
    );
  }
}

class _SettingsButton extends StatelessWidget {
  const _SettingsButton({
    required this.light,
    required this.focusNode,
    required this.downFocusNode,
    required this.onPressed,
  });

  final bool light;
  final FocusNode focusNode;
  final FocusNode downFocusNode;
  final VoidCallback onPressed;

  void _activateFromRemote() {
    if (focusNode.hasPrimaryFocus) onPressed();
  }

  @override
  Widget build(BuildContext context) => RemoteFocusFrame(
    autofocus: true,
    focusNode: focusNode,
    onActivate: _activateFromRemote,
    shortcuts: const <ShortcutActivator, Intent>{
      SingleActivator(LogicalKeyboardKey.arrowDown): DirectionalFocusIntent(
        TraversalDirection.down,
      ),
    },
    actions: <Type, Action<Intent>>{
      DirectionalFocusIntent: CallbackAction<DirectionalFocusIntent>(
        onInvoke: (_) {
          downFocusNode.requestFocus();
          return null;
        },
      ),
    },
    borderColor: light ? Colors.white : Colors.black,
    shape: const CircleBorder(),
    child: SizedBox.square(
      dimension: 42,
      child: Material(
        color: light
            ? Colors.black.withValues(alpha: .30)
            : Colors.white.withValues(alpha: .72),
        shape: const CircleBorder(),
        child: IconButton(
          padding: EdgeInsets.zero,
          tooltip: '设置',
          onPressed: onPressed,
          icon: FusionVectorIcon(
            FusionIcons.settings,
            size: 18,
            color: (light ? Colors.white : Colors.black).withValues(alpha: .88),
          ),
        ),
      ),
    ),
  );
}
