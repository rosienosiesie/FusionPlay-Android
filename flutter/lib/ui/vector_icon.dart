import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';

abstract final class FusionIcons {
  static const previous = 'assets/icon-skip-previous.svg';
  static const play = 'assets/icon-play-arrow.svg';
  static const pause = 'assets/icon-pause.svg';
  static const next = 'assets/icon-skip-next.svg';
}

class FusionVectorIcon extends StatelessWidget {
  const FusionVectorIcon(
    this.asset, {
    super.key,
    this.size = 24,
    this.color,
    this.semanticLabel,
  });

  final String asset;
  final double size;
  final Color? color;
  final String? semanticLabel;

  @override
  Widget build(BuildContext context) => SvgPicture.asset(
    asset,
    width: size,
    height: size,
    colorFilter: color == null
        ? null
        : ColorFilter.mode(color!, BlendMode.srcIn),
    semanticsLabel: semanticLabel,
    excludeFromSemantics: semanticLabel == null,
  );
}
