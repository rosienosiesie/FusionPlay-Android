import 'dart:math' as math;

class PlayerLayoutMetrics {
  const PlayerLayoutMetrics({
    required this.horizontal,
    required this.artworkSize,
    required this.contentGap,
    required this.detailsMinWidth,
    required this.detailsMaxWidth,
    required this.textScale,
    required this.spacingScale,
    required this.controlScale,
  });

  final bool horizontal;
  final double artworkSize;
  final double contentGap;
  final double detailsMinWidth;
  final double detailsMaxWidth;
  final double textScale;
  final double spacingScale;
  final double controlScale;
}

class PlayerViewportInsets {
  const PlayerViewportInsets({
    required this.horizontal,
    required this.vertical,
  });

  final double horizontal;
  final double vertical;
}

PlayerViewportInsets playerViewportInsets(
  double availableWidth,
  double availableHeight,
) {
  final shortEdge = math.min(availableWidth, availableHeight);
  return PlayerViewportInsets(
    horizontal: (shortEdge * .065).clamp(20, 56).toDouble(),
    vertical: (shortEdge * .045).clamp(14, 32).toDouble(),
  );
}

PlayerLayoutMetrics playerLayoutMetrics(
  double availableWidth,
  double availableHeight, {
  bool? landscape,
}) {
  final width = math.max(1.0, availableWidth);
  final height = math.max(1.0, availableHeight);
  final horizontal = landscape ?? width > height;
  if (horizontal) {
    final scale = math
        .min(width / 1168, height / 608)
        .clamp(.80, 1.60)
        .toDouble();
    final gap = (56 * scale).clamp(24, 84).toDouble();
    final detailsMin = (280 * math.min(scale, 1.25)).clamp(180, 350).toDouble();
    final rawDetailsMax = (430 * scale).clamp(330, 620).toDouble();
    final artwork = math.min(
      400 * scale,
      math.min(height * .80, math.max(1.0, width - gap - detailsMin)),
    );
    final detailsMax = math.min(
      rawDetailsMax,
      math.max(detailsMin, width - artwork - gap),
    );
    return PlayerLayoutMetrics(
      horizontal: true,
      artworkSize: artwork,
      contentGap: gap,
      detailsMinWidth: detailsMin,
      detailsMaxWidth: detailsMax,
      textScale: scale.clamp(.92, 1.35),
      spacingScale: scale.clamp(.88, 1.30),
      controlScale: scale.clamp(.94, 1.28),
    );
  }

  final portrait = landscape == false || (landscape == null && height >= width);
  if (portrait) {
    final scale = math
        .min(width / 360, height / 720)
        .clamp(.90, 1.15)
        .toDouble();
    final artwork = math.min(
      336 * scale,
      math.min(math.max(1.0, width - 8), math.max(1.0, height * .48)),
    );
    return PlayerLayoutMetrics(
      horizontal: false,
      artworkSize: artwork,
      contentGap: (18 * scale).clamp(16, 22).toDouble(),
      detailsMinWidth: 0,
      detailsMaxWidth: math.min(360 * scale, width),
      textScale: scale.clamp(.96, 1.12),
      spacingScale: scale.clamp(.92, 1.10),
      controlScale: (scale * 1.16).clamp(1.12, 1.24),
    );
  }

  final scale = math.min(width / 600, height / 700).clamp(.72, 1.25).toDouble();
  final artwork = math.min(
    300 * scale,
    math.min(math.max(1.0, width - 32), math.max(1.0, height * .46)),
  );
  return PlayerLayoutMetrics(
    horizontal: false,
    artworkSize: artwork,
    contentGap: (20 * scale).clamp(14, 28),
    detailsMinWidth: 0,
    detailsMaxWidth: math.min(430 * scale, width),
    textScale: scale.clamp(.90, 1.18),
    spacingScale: scale.clamp(.84, 1.18),
    controlScale: scale.clamp(.88, 1.15),
  );
}
