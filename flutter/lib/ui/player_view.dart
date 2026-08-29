import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../models/app_state.dart';
import '../theme/fusion_theme.dart';
import 'animated_press_scale.dart';
import 'artwork.dart';
import 'g2_shape.dart';
import 'player_layout.dart';
import 'rounded_progress_bar.dart';
import 'remote_focus.dart';
import 'vector_icon.dart';

class PlayerView extends StatefulWidget {
  const PlayerView({
    super.key,
    required this.state,
    required this.onPlayback,
    required this.onSeek,
    required this.onVolume,
    this.artworkSource,
    this.artworkIdentity,
    this.progressFocusNode,
    this.settingsFocusNode,
  });
  final AppState state;
  final ValueChanged<PlaybackCommand> onPlayback;
  final ValueChanged<int> onSeek;
  final ValueChanged<int> onVolume;
  final String? artworkSource;
  final String? artworkIdentity;
  final FocusNode? progressFocusNode;
  final FocusNode? settingsFocusNode;

  @override
  State<PlayerView> createState() => _PlayerViewState();
}

class _PlayerViewState extends State<PlayerView> {
  final FocusNode _fallbackProgressFocus = FocusNode(
    debugLabel: 'player-progress',
  );
  final FocusNode _previousFocus = FocusNode(debugLabel: 'player-previous');
  final FocusNode _playPauseFocus = FocusNode(debugLabel: 'player-play-pause');
  final FocusNode _nextFocus = FocusNode(debugLabel: 'player-next');
  final ValueNotifier<bool> _previousPressed = ValueNotifier(false);
  final ValueNotifier<bool> _playPausePressed = ValueNotifier(false);
  final ValueNotifier<bool> _nextPressed = ValueNotifier(false);
  bool seeking = false;
  double? seekPositionMs;
  int? pendingSeekTargetMs;
  Timer? pendingSeekTimer;
  ArtworkFlipDirection artworkFlipDirection = ArtworkFlipDirection.right;
  bool artworkFlipDirectionPending = false;
  Timer? artworkFlipDirectionTimer;
  final List<String> _artworkHistory = [];
  int _artworkHistoryIndex = -1;

  FocusNode get _progressFocus =>
      widget.progressFocusNode ?? _fallbackProgressFocus;

  @override
  void initState() {
    super.initState();
    final identity = _artworkHistoryIdentity(widget);
    if (identity != null) {
      _artworkHistory.add(identity);
      _artworkHistoryIndex = 0;
    }
  }

  @override
  void didUpdateWidget(covariant PlayerView oldWidget) {
    super.didUpdateWidget(oldWidget);
    final artworkTransitionChanged = widget.artworkIdentity != null
        ? widget.artworkIdentity != oldWidget.artworkIdentity
        : widget.artworkSource != oldWidget.artworkSource;
    if (artworkTransitionChanged) {
      final identity = _artworkHistoryIdentity(widget);
      if (artworkFlipDirectionPending) {
        if (identity != null) {
          _recordArtworkDirection(identity, artworkFlipDirection);
        }
      } else {
        final reportedDirection =
            switch (widget.state.playback.trackChangeDirection) {
              TrackChangeDirection.previous => ArtworkFlipDirection.right,
              TrackChangeDirection.next => ArtworkFlipDirection.left,
              null => null,
            };
        artworkFlipDirection =
            reportedDirection ??
            (identity == null
                ? ArtworkFlipDirection.left
                : _inferArtworkDirection(identity));
        if (identity != null && reportedDirection != null) {
          _recordArtworkDirection(identity, reportedDirection);
        }
      }
      artworkFlipDirectionPending = false;
      artworkFlipDirectionTimer?.cancel();
    }
    if (!seeking) {
      final pending = pendingSeekTargetMs;
      if (pending == null ||
          (widget.state.playback.positionMs - pending).abs() <= 1500) {
        pendingSeekTimer?.cancel();
        pendingSeekTargetMs = null;
        seekPositionMs = widget.state.playback.positionMs.toDouble();
      } else {
        seekPositionMs = pending.toDouble();
      }
    }
  }

  String? _artworkHistoryIdentity(PlayerView view) {
    final identity = (view.artworkIdentity ?? view.artworkSource)?.trim();
    if (identity == null || identity.isEmpty) return null;
    final protocol = view.state.playback.protocol?.trim().toLowerCase() ?? '';
    return '$protocol\u001f$identity';
  }

  ArtworkFlipDirection _inferArtworkDirection(String identity) {
    if (_artworkHistoryIndex < 0 || _artworkHistory.isEmpty) {
      _artworkHistory
        ..clear()
        ..add(identity);
      _artworkHistoryIndex = 0;
      return ArtworkFlipDirection.left;
    }

    for (var index = _artworkHistoryIndex - 1; index >= 0; index--) {
      if (_artworkHistory[index] == identity) {
        _artworkHistoryIndex = index;
        return ArtworkFlipDirection.right;
      }
    }
    for (
      var index = _artworkHistoryIndex + 1;
      index < _artworkHistory.length;
      index++
    ) {
      if (_artworkHistory[index] == identity) {
        _artworkHistoryIndex = index;
        return ArtworkFlipDirection.left;
      }
    }

    _recordArtworkDirection(identity, ArtworkFlipDirection.left);
    return ArtworkFlipDirection.left;
  }

  void _recordArtworkDirection(
    String identity,
    ArtworkFlipDirection direction,
  ) {
    if (_artworkHistoryIndex < 0 || _artworkHistory.isEmpty) {
      _artworkHistory
        ..clear()
        ..add(identity);
      _artworkHistoryIndex = 0;
      return;
    }
    if (_artworkHistory[_artworkHistoryIndex] == identity) return;

    if (direction == ArtworkFlipDirection.right) {
      if (_artworkHistoryIndex > 0 &&
          _artworkHistory[_artworkHistoryIndex - 1] == identity) {
        _artworkHistoryIndex--;
      } else {
        _artworkHistory.insert(_artworkHistoryIndex, identity);
      }
    } else if (_artworkHistoryIndex + 1 < _artworkHistory.length &&
        _artworkHistory[_artworkHistoryIndex + 1] == identity) {
      _artworkHistoryIndex++;
    } else {
      _artworkHistory.removeRange(
        _artworkHistoryIndex + 1,
        _artworkHistory.length,
      );
      _artworkHistory.add(identity);
      _artworkHistoryIndex++;
    }

    const maximumHistoryLength = 32;
    if (_artworkHistory.length > maximumHistoryLength) {
      final overflow = _artworkHistory.length - maximumHistoryLength;
      _artworkHistory.removeRange(0, overflow);
      _artworkHistoryIndex = (_artworkHistoryIndex - overflow).clamp(
        0,
        _artworkHistory.length - 1,
      );
    }
  }

  @override
  void dispose() {
    pendingSeekTimer?.cancel();
    artworkFlipDirectionTimer?.cancel();
    _fallbackProgressFocus.dispose();
    _previousFocus.dispose();
    _playPauseFocus.dispose();
    _nextFocus.dispose();
    _previousPressed.dispose();
    _playPausePressed.dispose();
    _nextPressed.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, outer) {
        final landscape = outer.maxWidth > outer.maxHeight;
        final portrait = !landscape;
        final viewportInsets = playerViewportInsets(
          outer.maxWidth,
          outer.maxHeight,
        );
        final sidePadding = portrait ? 16.0 : viewportInsets.horizontal;
        final verticalPadding = portrait ? 16.0 : viewportInsets.vertical;
        return Padding(
          padding: EdgeInsets.fromLTRB(
            sidePadding,
            verticalPadding,
            sidePadding,
            verticalPadding,
          ),
          child: LayoutBuilder(
            builder: (context, box) {
              final layout = playerLayoutMetrics(
                box.maxWidth,
                box.maxHeight,
                landscape: landscape,
              );
              return layout.horizontal
                  ? _horizontal(layout)
                  : _compact(layout, centerTrackText: portrait);
            },
          ),
        );
      },
    );
  }

  Widget _horizontal(PlayerLayoutMetrics layout) {
    return Center(
      child: Row(
        key: const ValueKey('player-layout-horizontal'),
        mainAxisSize: MainAxisSize.min,
        children: [
          ArtworkCard(
            source: widget.artworkSource,
            size: layout.artworkSize,
            hasPlayback: _hasPlayback,
            identity: widget.artworkIdentity,
            flipDirection: artworkFlipDirection,
          ),
          SizedBox(width: layout.contentGap),
          ConstrainedBox(
            constraints: BoxConstraints(
              minWidth: layout.detailsMinWidth,
              maxWidth: layout.detailsMaxWidth,
            ),
            child: _details(
              textScale: layout.textScale,
              spacingScale: layout.spacingScale,
              controlScale: layout.controlScale,
              contentGap: layout.contentGap,
            ),
          ),
        ],
      ),
    );
  }

  Widget _compact(PlayerLayoutMetrics layout, {required bool centerTrackText}) {
    return Center(
      child: SingleChildScrollView(
        child: Column(
          key: const ValueKey('player-layout-compact'),
          mainAxisSize: MainAxisSize.min,
          children: [
            ArtworkCard(
              source: widget.artworkSource,
              size: layout.artworkSize,
              hasPlayback: _hasPlayback,
              identity: widget.artworkIdentity,
              flipDirection: artworkFlipDirection,
            ),
            SizedBox(height: layout.contentGap),
            SizedBox(
              width: layout.detailsMaxWidth,
              child: _details(
                textScale: layout.textScale,
                spacingScale: layout.spacingScale,
                controlScale: layout.controlScale,
                contentGap: layout.contentGap,
                centerTrackText: centerTrackText,
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _details({
    required double textScale,
    required double spacingScale,
    required double controlScale,
    required double contentGap,
    bool centerTrackText = false,
  }) {
    final playback = widget.state.playback;
    final hasArtwork = widget.artworkSource?.trim().isNotEmpty ?? false;
    final hasPlayback = _hasPlayback;
    final foreground = hasArtwork ? Colors.white : const Color(0xFF171717);
    final muted = hasArtwork
        ? Colors.white.withValues(alpha: .74)
        : const Color(0xFF5F5F5F);
    final duration = math.max(0, playback.durationMs ?? 0);
    final position = (seekPositionMs?.round() ?? playback.positionMs).clamp(
      0,
      duration == 0 ? playback.positionMs : duration,
    );
    final canSeek = duration > 0 && _can('seek');
    final progress = duration > 0 ? position / duration : 0.0;
    return Column(
      crossAxisAlignment: centerTrackText
          ? CrossAxisAlignment.center
          : CrossAxisAlignment.stretch,
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          playback.title?.trim().isNotEmpty == true
              ? playback.title!.trim()
              : '暂无音乐',
          maxLines: 2,
          overflow: TextOverflow.ellipsis,
          textAlign: centerTrackText ? TextAlign.center : TextAlign.start,
          style: Theme.of(context).textTheme.headlineMedium?.copyWith(
            fontSize: 28 * textScale,
            color: foreground,
            fontWeight: FusionTypography.semiBoldFontWeight,
            fontVariations: FusionTypography.semiBold,
          ),
        ),
        if (playback.artist?.trim().isNotEmpty == true) ...[
          SizedBox(height: 6 * spacingScale),
          Text(
            playback.artist!.trim(),
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            textAlign: centerTrackText ? TextAlign.center : TextAlign.start,
            style: Theme.of(context).textTheme.titleMedium?.copyWith(
              fontSize: 16 * textScale,
              color: muted,
            ),
          ),
        ],
        SizedBox(height: centerTrackText ? contentGap : 30 * spacingScale),
        Row(
          children: [
            SizedBox(
              width: 48 * spacingScale,
              child: Transform.translate(
                offset: Offset(centerTrackText ? -4 : 0, 0),
                child: Text(
                  hasPlayback ? _duration(position) : '1:14',
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    color: muted,
                    fontSize: 12 * textScale,
                    fontWeight: FusionTypography.mediumFontWeight,
                    fontVariations: FusionTypography.medium,
                  ),
                ),
              ),
            ),
            Expanded(
              child: RoundedProgressBar(
                value: progress,
                enabled: duration > 0,
                seekEnabled: canSeek,
                showThumb: false,
                focusNode: _progressFocus,
                upFocusNode: widget.settingsFocusNode,
                downFocusNode: _playPauseFocus,
                focusBorderColor: hasArtwork ? Colors.white : Colors.black,
                activeColor: hasPlayback ? foreground : const Color(0x1A1A1A1A),
                inactiveColor: duration <= 0
                    ? (!hasPlayback
                          ? const Color(0x1A1A1A1A)
                          : hasArtwork
                          ? Colors.white.withValues(alpha: .56)
                          : const Color(0x331A1A1A))
                    : (hasArtwork
                          ? Colors.white.withValues(alpha: .28)
                          : const Color(0xFFE7E7E7)),
                onChangeStart: (_) => setState(() => seeking = true),
                onChanged: (value) => setState(() {
                  seeking = true;
                  seekPositionMs = duration * value;
                }),
                onChangeEnd: (value) {
                  final target = (duration * value).round();
                  setState(() {
                    seeking = false;
                    seekPositionMs = target.toDouble();
                    pendingSeekTargetMs = target;
                  });
                  pendingSeekTimer?.cancel();
                  pendingSeekTimer = Timer(
                    const Duration(milliseconds: 3000),
                    () {
                      if (!mounted || pendingSeekTargetMs != target) return;
                      setState(() {
                        pendingSeekTargetMs = null;
                        seekPositionMs = widget.state.playback.positionMs
                            .toDouble();
                      });
                    },
                  );
                  widget.onSeek(target);
                },
              ),
            ),
            SizedBox(
              width: 48 * spacingScale,
              child: Transform.translate(
                offset: Offset(centerTrackText ? 4 : 0, 0),
                child: Text(
                  hasPlayback ? _duration(duration) : '5:14',
                  textAlign: TextAlign.center,
                  style: TextStyle(
                    color: muted,
                    fontSize: 12 * textScale,
                    fontWeight: FusionTypography.mediumFontWeight,
                    fontVariations: FusionTypography.medium,
                  ),
                ),
              ),
            ),
          ],
        ),
        SizedBox(height: 20 * spacingScale),
        _controls(controlScale, spacingScale, hasPlayback),
      ],
    );
  }

  Widget _controls(double controlScale, double spacingScale, bool hasPlayback) {
    final small = 50 * controlScale;
    final play = 62 * controlScale;
    final gap = 18 * spacingScale;
    const idleElement = Color(0x1A1A1A1A);
    final tonal = hasPlayback
        ? Colors.black.withValues(alpha: .28)
        : idleElement;
    final disabledContainer = hasPlayback
        ? Colors.black.withValues(alpha: .28)
        : idleElement;
    final disabledContent = hasPlayback
        ? Colors.white.withValues(alpha: .38)
        : const Color(0x61151515);
    final playBackground = hasPlayback ? Colors.white : idleElement;
    final playForeground = hasPlayback ? Colors.black : const Color(0xFF151515);
    final playDisabledBackground = hasPlayback ? Colors.white : idleElement;
    final playDisabledForeground = const Color(0x61151515);
    final smallForeground = hasPlayback
        ? Colors.white
        : const Color(0xFF151515);
    final coreControls = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        _controlButton(
          FusionIcons.previous,
          _previousFocus,
          _previousPressed,
          small,
          24 * controlScale,
          tonal,
          smallForeground,
          disabledContainer,
          disabledContent,
          hasPlayback
              ? () => _changeTrack(
                  PlaybackCommand.previous,
                  ArtworkFlipDirection.right,
                )
              : null,
          controlKey: const ValueKey('playback-previous-button'),
        ),
        SizedBox(width: gap),
        _controlButton(
          widget.state.playback.isPlaying
              ? FusionIcons.pause
              : FusionIcons.play,
          _playPauseFocus,
          _playPausePressed,
          play,
          30 * controlScale,
          playBackground,
          playForeground,
          playDisabledBackground,
          playDisabledForeground,
          hasPlayback
              ? () => widget.onPlayback(PlaybackCommand.playPause)
              : null,
          upFocusNode: _progressFocus,
          leftFocusNode: _previousFocus,
          rightFocusNode: _nextFocus,
          controlKey: const ValueKey('playback-play-pause-button'),
        ),
        SizedBox(width: gap),
        _controlButton(
          FusionIcons.next,
          _nextFocus,
          _nextPressed,
          small,
          24 * controlScale,
          tonal,
          smallForeground,
          disabledContainer,
          disabledContent,
          hasPlayback
              ? () => _changeTrack(
                  PlaybackCommand.next,
                  ArtworkFlipDirection.left,
                )
              : null,
          controlKey: const ValueKey('playback-next-button'),
        ),
      ],
    );
    return SizedBox(
      height: play,
      child: Center(child: coreControls),
    );
  }

  void _changeTrack(PlaybackCommand command, ArtworkFlipDirection direction) {
    setState(() {
      artworkFlipDirection = direction;
      artworkFlipDirectionPending = true;
    });
    artworkFlipDirectionTimer?.cancel();
    artworkFlipDirectionTimer = Timer(const Duration(seconds: 3), () {
      if (!mounted || !artworkFlipDirectionPending) return;
      setState(() {
        artworkFlipDirection = ArtworkFlipDirection.right;
        artworkFlipDirectionPending = false;
      });
    });
    widget.onPlayback(command);
  }

  Widget _controlButton(
    String icon,
    FocusNode focusNode,
    ValueNotifier<bool> remotePressed,
    double size,
    double iconSize,
    Color background,
    Color foreground,
    Color disabledBackground,
    Color disabledForeground,
    VoidCallback? onPressed, {
    required Key controlKey,
    FocusNode? upFocusNode,
    FocusNode? leftFocusNode,
    FocusNode? rightFocusNode,
  }) {
    final lightFocus = widget.artworkSource?.trim().isNotEmpty ?? false;
    final hasDirectionalTargets =
        upFocusNode != null || leftFocusNode != null || rightFocusNode != null;
    return AnimatedPressScale(
      key: controlKey,
      enabled: onPressed != null,
      externalPressed: remotePressed,
      child: RemoteFocusFrame(
        enabled: onPressed != null,
        focusNode: focusNode,
        onActivate: onPressed,
        onPressChange: (pressed) => remotePressed.value = pressed,
        shortcuts: <ShortcutActivator, Intent>{
          if (upFocusNode != null)
            const SingleActivator(LogicalKeyboardKey.arrowUp):
                const DirectionalFocusIntent(TraversalDirection.up),
          if (leftFocusNode != null)
            const SingleActivator(LogicalKeyboardKey.arrowLeft):
                const DirectionalFocusIntent(TraversalDirection.left),
          if (rightFocusNode != null)
            const SingleActivator(LogicalKeyboardKey.arrowRight):
                const DirectionalFocusIntent(TraversalDirection.right),
        },
        actions: !hasDirectionalTargets
            ? const <Type, Action<Intent>>{}
            : <Type, Action<Intent>>{
                DirectionalFocusIntent: CallbackAction<DirectionalFocusIntent>(
                  onInvoke: (intent) {
                    final target = switch (intent.direction) {
                      TraversalDirection.up => upFocusNode,
                      TraversalDirection.left => leftFocusNode,
                      TraversalDirection.right => rightFocusNode,
                      _ => null,
                    };
                    target?.requestFocus();
                    return null;
                  },
                ),
              },
        borderColor: lightFocus ? Colors.white : Colors.black,
        shape: G2ContinuousBorder(radius: size / 2),
        child: SizedBox.square(
          dimension: size,
          child: Material(
            color: onPressed == null ? disabledBackground : background,
            shape: G2ContinuousBorder(radius: size / 2),
            clipBehavior: Clip.antiAlias,
            child: IconButton(
              padding: EdgeInsets.zero,
              onPressed: onPressed,
              icon: FusionVectorIcon(
                icon,
                size: iconSize,
                color: onPressed == null ? disabledForeground : foreground,
              ),
            ),
          ),
        ),
      ),
    );
  }

  bool _can(String command) {
    if (!widget.state.remoteControl.available) return false;
    final commands = widget.state.remoteControl.commands;
    return commands.contains(command) ||
        (command == 'previous' && commands.contains('previous_track')) ||
        (command == 'next' && commands.contains('next_track'));
  }

  bool get _hasPlayback {
    final playback = widget.state.playback;
    return playback.streamActive ||
        (playback.protocol?.trim().isNotEmpty ?? false) ||
        (playback.mediaUrl?.trim().isNotEmpty ?? false) ||
        (playback.title?.trim().isNotEmpty ?? false) ||
        (playback.artist?.trim().isNotEmpty ?? false) ||
        playback.durationMs != null;
  }

  String _duration(int milliseconds) {
    final seconds = milliseconds ~/ 1000;
    return '${seconds ~/ 60}:${(seconds % 60).toString().padLeft(2, '0')}';
  }
}
