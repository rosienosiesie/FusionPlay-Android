import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math' as math;
import 'dart:ui';

import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';

import '../theme/fusion_theme.dart';

const _artworkProviderCacheLimit = 4;
const _artworkDecodeSize = 1024;
const _maximumDataArtworkCharacters = 12 * 1024 * 1024;
const artworkClearGracePeriod = Duration(milliseconds: 1200);
final _artworkProviderCache = <String, ImageProvider<Object>>{};

ImageProvider<Object>? artworkProvider(String? source) {
  final value = source?.trim();
  if (value == null || value.isEmpty) return null;
  final cached = _artworkProviderCache.remove(value);
  if (cached != null) {
    _artworkProviderCache[value] = cached;
    return cached;
  }
  try {
    late final ImageProvider<Object> sourceProvider;
    if (value.startsWith('data:')) {
      final comma = value.indexOf(',');
      if (comma < 0 ||
          value.length - comma - 1 > _maximumDataArtworkCharacters) {
        return null;
      }
      sourceProvider = MemoryImage(base64Decode(value.substring(comma + 1)));
    } else if (value.startsWith('http://') || value.startsWith('https://')) {
      sourceProvider = NetworkImage(value);
    } else if (value.startsWith('file:')) {
      sourceProvider = FileImage(File(Uri.parse(value).toFilePath()));
    } else {
      sourceProvider = FileImage(File(value));
    }
    final provider = ResizeImage.resizeIfNeeded(
      _artworkDecodeSize,
      _artworkDecodeSize,
      sourceProvider,
    );
    _artworkProviderCache[value] = provider;
    while (_artworkProviderCache.length > _artworkProviderCacheLimit) {
      final evicted = _artworkProviderCache.remove(
        _artworkProviderCache.keys.first,
      );
      if (evicted != null) unawaited(evicted.evict());
    }
    return provider;
  } catch (_) {
    return null;
  }
}

class ArtworkBackground extends StatelessWidget {
  const ArtworkBackground({
    super.key,
    required this.source,
    required this.hasPlayback,
  });
  static const transitionDuration = Duration(milliseconds: 260);
  static const transitionCurve = Cubic(.77, 0, .175, 1);

  final String? source;
  final bool hasPlayback;

  @override
  Widget build(BuildContext context) {
    final sourceKey = source?.trim();
    final provider = artworkProvider(sourceKey);
    final layerKey = ValueKey<String>(
      sourceKey == null || sourceKey.isEmpty
          ? 'artwork-background:fallback:$hasPlayback'
          : 'artwork-background:$sourceKey',
    );
    final reduceMotion =
        MediaQuery.maybeOf(context)?.disableAnimations ?? false;
    return AnimatedSwitcher(
      duration: reduceMotion
          ? Duration.zero
          : ArtworkBackground.transitionDuration,
      switchInCurve: transitionCurve,
      switchOutCurve: transitionCurve,
      layoutBuilder: (current, previous) =>
          Stack(fit: StackFit.expand, children: [...previous, ?current]),
      // Keep the outgoing frame fully opaque and fade the prepared incoming
      // frame over it. A symmetric cross-fade exposes the black base at its
      // midpoint and creates the visible brightness flash.
      transitionBuilder: (child, animation) {
        final childKey = child.key;
        return FadeTransition(
          key: childKey is ValueKey<String>
              ? ValueKey<String>('artwork-fade:${childKey.value}')
              : null,
          opacity: _IncomingArtworkOpacity(animation),
          child: child,
        );
      },
      child: _ArtworkBackgroundLayer(
        key: layerKey,
        provider: provider,
        hasPlayback: hasPlayback,
      ),
    );
  }
}

class _IncomingArtworkOpacity extends Animation<double>
    with AnimationWithParentMixin<double> {
  _IncomingArtworkOpacity(this.parent);

  @override
  final Animation<double> parent;

  @override
  double get value =>
      parent.status == AnimationStatus.reverse ? 1 : parent.value;
}

class _ArtworkBackgroundLayer extends StatelessWidget {
  const _ArtworkBackgroundLayer({
    super.key,
    required this.provider,
    required this.hasPlayback,
  });

  final ImageProvider<Object>? provider;
  final bool hasPlayback;

  @override
  Widget build(BuildContext context) {
    final advancedEffectsEnabled =
        !(MediaQuery.maybeOf(context)?.disableAnimations ?? false);
    final artworkImage = provider == null
        ? null
        : Image(image: provider!, fit: BoxFit.cover, gaplessPlayback: true);
    return DecoratedBox(
      decoration: BoxDecoration(
        color: provider == null && !hasPlayback
            ? FusionColors.playerFallback
            : Colors.black,
        gradient: provider == null && hasPlayback
            ? const LinearGradient(
                begin: Alignment.topLeft,
                end: Alignment.bottomRight,
                colors: [
                  Color(0xFFF1F1F1),
                  Color(0xFFD3D3D3),
                  Color(0xFFE7E7E7),
                ],
              )
            : null,
      ),
      child: Stack(
        fit: StackFit.expand,
        children: [
          if (artworkImage != null)
            advancedEffectsEnabled
                ? ImageFiltered(
                    imageFilter: ImageFilter.blur(
                      sigmaX: 76,
                      sigmaY: 76,
                      tileMode: TileMode.clamp,
                    ),
                    child: artworkImage,
                  )
                : artworkImage,
          if (provider != null)
            const DecoratedBox(
              decoration: BoxDecoration(
                gradient: LinearGradient(
                  begin: Alignment.topCenter,
                  end: Alignment.bottomCenter,
                  colors: [
                    Color.fromRGBO(0, 0, 0, .20),
                    Color.fromRGBO(0, 0, 0, .26),
                    Color.fromRGBO(0, 0, 0, .34),
                  ],
                  stops: [0, .52, 1],
                ),
              ),
            ),
          if (provider != null)
            const ColoredBox(color: Color.fromRGBO(255, 255, 255, .15)),
        ],
      ),
    );
  }
}

class ArtworkFlipFrame {
  const ArtworkFlipFrame({
    required this.showIncoming,
    required this.rotationY,
    required this.opacity,
  });

  final bool showIncoming;
  final double rotationY;
  final double opacity;
}

enum ArtworkFlipDirection { left, right }

ArtworkFlipFrame artworkFlipFrame(
  double progress, {
  required bool reducedMotion,
  ArtworkFlipDirection direction = ArtworkFlipDirection.right,
}) {
  final clamped = progress.clamp(0.0, 1.0);
  final incoming = clamped >= .5;
  final halfProgress = incoming ? (clamped - .5) * 2 : clamped * 2;
  if (reducedMotion) {
    return ArtworkFlipFrame(
      showIncoming: incoming,
      rotationY: 0,
      opacity: incoming ? halfProgress : 1 - halfProgress,
    );
  }
  final directionSign = direction == ArtworkFlipDirection.right ? 1.0 : -1.0;
  return ArtworkFlipFrame(
    showIncoming: incoming,
    rotationY: incoming
        ? directionSign * (-math.pi / 2 + math.pi / 2 * halfProgress)
        : directionSign * math.pi / 2 * halfProgress,
    opacity: 1,
  );
}

class ArtworkCard extends StatefulWidget {
  const ArtworkCard({
    super.key,
    required this.source,
    required this.size,
    required this.hasPlayback,
    this.identity,
    this.flipDirection = ArtworkFlipDirection.right,
  });
  static const flipDuration = Duration(milliseconds: 240);
  static const reducedMotionDuration = Duration(milliseconds: 160);
  static const flipCurve = Cubic(.77, 0, .175, 1);

  final String? source;
  final double size;
  final bool hasPlayback;
  final String? identity;
  final ArtworkFlipDirection flipDirection;

  @override
  State<ArtworkCard> createState() => _ArtworkCardState();
}

class _ArtworkCardState extends State<ArtworkCard>
    with SingleTickerProviderStateMixin {
  late final AnimationController controller;
  late Object? activeIdentity;
  String? outgoingSource;
  String? incomingSource;
  late ArtworkFlipDirection activeFlipDirection;
  bool reducedMotion = false;

  @override
  void initState() {
    super.initState();
    activeIdentity = _effectiveIdentity(widget);
    outgoingSource = widget.source;
    incomingSource = widget.source;
    activeFlipDirection = widget.flipDirection;
    controller = AnimationController(vsync: this, value: 1)..addListener(_tick);
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final nextReducedMotion =
        MediaQuery.maybeOf(context)?.disableAnimations ?? false;
    if (nextReducedMotion && !reducedMotion && !controller.isCompleted) {
      controller.value = 1;
    }
    reducedMotion = nextReducedMotion;
  }

  @override
  void didUpdateWidget(covariant ArtworkCard oldWidget) {
    super.didUpdateWidget(oldWidget);
    final nextIdentity = _effectiveIdentity(widget);
    if (nextIdentity != activeIdentity) {
      final visibleSource = controller.value < .5
          ? outgoingSource
          : incomingSource;
      outgoingSource = visibleSource;
      incomingSource = widget.source;
      activeFlipDirection = widget.flipDirection;
      activeIdentity = nextIdentity;
      if (reducedMotion) {
        controller.value = 1;
      } else {
        controller.duration = ArtworkCard.flipDuration;
        controller.forward(from: 0);
      }
    } else if (widget.source != incomingSource) {
      incomingSource = widget.source;
      if (controller.isCompleted) outgoingSource = widget.source;
    }
  }

  void _tick() {
    if (mounted) setState(() {});
  }

  Object? _effectiveIdentity(ArtworkCard card) {
    final identity = card.identity?.trim();
    if (identity != null && identity.isNotEmpty) return identity;
    final source = card.source?.trim();
    return source == null || source.isEmpty ? null : 'artwork:$source';
  }

  @override
  void dispose() {
    controller
      ..removeListener(_tick)
      ..dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final easedProgress = ArtworkCard.flipCurve.transform(controller.value);
    final frame = artworkFlipFrame(
      easedProgress,
      reducedMotion: reducedMotion,
      direction: activeFlipDirection,
    );
    final visibleSource = frame.showIncoming ? incomingSource : outgoingSource;
    final provider = artworkProvider(visibleSource);
    final transform = Matrix4.identity()
      ..setEntry(3, 2, .0012)
      ..rotateY(frame.rotationY);
    return SizedBox.square(
      dimension: widget.size,
      child: Opacity(
        opacity: frame.opacity,
        child: Transform(
          alignment: Alignment.center,
          transform: transform,
          child: RepaintBoundary(
            child: provider == null
                ? ColoredBox(
                    color: widget.hasPlayback
                        ? const Color(0xFFB8B8B8)
                        : const Color(0x1A1A1A1A),
                    child: Center(
                      child: SvgPicture.asset(
                        'assets/music-note-placeholder.svg',
                        width: (widget.size * .22).clamp(72, 128),
                        height: (widget.size * .22).clamp(72, 128),
                        colorFilter: const ColorFilter.mode(
                          Color(0xFF565656),
                          BlendMode.srcIn,
                        ),
                      ),
                    ),
                  )
                : Image(
                    image: provider,
                    fit: BoxFit.cover,
                    gaplessPlayback: true,
                    errorBuilder: (_, _, _) =>
                        const ColoredBox(color: Color(0x1A1A1A1A)),
                  ),
          ),
        ),
      ),
    );
  }
}
