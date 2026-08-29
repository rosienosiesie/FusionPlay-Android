import 'dart:math' as math;

import 'package:flutter/material.dart';

/// Exact uniform-radius port of Kyant Capsule's G2 continuity profile used by
/// the previous Android frontend.
class G2ContinuousBorder extends OutlinedBorder {
  const G2ContinuousBorder({this.radius = 24, super.side});
  final double radius;

  @override
  OutlinedBorder copyWith({BorderSide? side, double? radius}) =>
      G2ContinuousBorder(
        side: side ?? this.side,
        radius: radius ?? this.radius,
      );
  @override
  EdgeInsetsGeometry get dimensions => EdgeInsets.all(side.width);
  @override
  Path getInnerPath(Rect rect, {TextDirection? textDirection}) =>
      g2ContinuousRoundedRectPath(
        rect.deflate(side.width),
        math.max(0, radius - side.width),
      );
  @override
  Path getOuterPath(Rect rect, {TextDirection? textDirection}) =>
      g2ContinuousRoundedRectPath(rect, radius);
  @override
  void paint(Canvas canvas, Rect rect, {TextDirection? textDirection}) {
    if (side.style == BorderStyle.none || side.width == 0) return;
    canvas.drawPath(
      getOuterPath(rect, textDirection: textDirection),
      side.toPaint()..style = PaintingStyle.stroke,
    );
  }

  @override
  ShapeBorder scale(double t) =>
      G2ContinuousBorder(radius: radius * t, side: side.scale(t));
}

class G2Clipper extends CustomClipper<Path> {
  const G2Clipper(this.radius);
  final double radius;
  @override
  Path getClip(Size size) =>
      g2ContinuousRoundedRectPath(Offset.zero & size, radius);
  @override
  bool shouldReclip(G2Clipper oldClipper) => oldClipper.radius != radius;
}

Path g2ContinuousRoundedRectPath(Rect rect, double requestedRadius) {
  final path = Path();
  final width = rect.width;
  final height = rect.height;
  if (width <= 0 || height <= 0) return path;
  final radius = requestedRadius.clamp(0.0, math.min(width, height) * .5);
  final builder = _G2PathBuilder(path, rect.topLeft);
  final capsule = radius * 2 == width || radius * 2 == height;
  if (capsule && width > height) {
    builder.appendHorizontalCapsule(width, height);
  } else if (capsule && width < height) {
    builder.appendVerticalCapsule(width, height);
  } else if (capsule) {
    path.addOval(rect);
  } else {
    builder.appendRoundedRectangle(width, height, radius);
  }
  return path;
}

class _G2PathBuilder {
  _G2PathBuilder(this.path, this.origin);
  final Path path;
  final Offset origin;
  void moveTo(double x, double y) => path.moveTo(origin.dx + x, origin.dy + y);
  void lineTo(double x, double y) => path.lineTo(origin.dx + x, origin.dy + y);
  void cubicTo(
    double x1,
    double y1,
    double x2,
    double y2,
    double x3,
    double y3,
  ) => path.cubicTo(
    origin.dx + x1,
    origin.dy + y1,
    origin.dx + x2,
    origin.dy + y2,
    origin.dx + x3,
    origin.dy + y3,
  );
  void arcTo(_G2Point center, double radius, double start, double sweep) {
    path.arcTo(
      Rect.fromCircle(
        center: Offset(origin.dx + center.x, origin.dy + center.y),
        radius: radius,
      ),
      start,
      sweep,
      false,
    );
  }

  void arcToScaled(
    _G2Point center,
    double radius,
    double radiusScale,
    double start,
    double sweep,
  ) {
    final centerAngle = start + sweep * .5;
    arcTo(
      center +
          _G2Point(math.cos(centerAngle), math.sin(centerAngle)) *
              radius *
              (1 - radiusScale),
      radius * radiusScale,
      start,
      sweep,
    );
  }

  void appendRoundedRectangle(double width, double height, double radius) {
    final centerX = width * .5;
    final centerY = height * .5;
    const rounded = _G2Profile.roundedRectangle;
    const capsule = _G2Profile.capsule;
    final ratioV = _cornerRatio(centerY, radius, rounded.extendedFraction);
    final ratioH = _cornerRatio(centerX, radius, rounded.extendedFraction);
    final ratio = math.min(ratioV, ratioH);
    final extended = _lerp(
      capsule.extendedFraction,
      rounded.extendedFraction,
      ratio,
    );
    final extV = extended * ratioV;
    final extH = extended * ratioH;
    final offsetV = -radius * extV;
    final offsetH = -radius * extH;
    final bezierScale = _lerp(
      capsule.bezierCurvatureScale,
      rounded.bezierCurvatureScale,
      ratio,
    );
    final arcFraction = _lerp(capsule.arcFraction, rounded.arcFraction, ratio);
    final arcScale = 1 + (rounded.arcCurvatureScale - 1) * ratio;
    final bezierV = _G2Profile(extV, arcFraction, bezierScale, arcScale).bezier;
    final bezierH = _G2Profile(extH, arcFraction, bezierScale, arcScale).bezier;
    final radiusScale = 1 / arcScale;
    final bezierArc = math.pi * .5 * (1 - arcFraction) * .5;
    final arcSweep = math.pi * .5 * arcFraction;

    var x = 0.0;
    var y = radius;
    moveTo(x, y - offsetV);
    if (radius > 0) {
      cubicTo(
        x + bezierV.p1.y * radius,
        y - bezierV.p1.x * radius,
        x + bezierV.p2.y * radius,
        y - bezierV.p2.x * radius,
        x + bezierV.p3.y * radius,
        y - bezierV.p3.x * radius,
      );
      arcToScaled(
        _G2Point(radius, radius),
        radius,
        radiusScale,
        math.pi + bezierArc,
        arcSweep,
      );
      x = radius;
      y = 0;
      cubicTo(
        x - bezierH.p2.x * radius,
        y + bezierH.p2.y * radius,
        x - bezierH.p1.x * radius,
        y + bezierH.p1.y * radius,
        x - math.max(bezierH.p0.x * radius, offsetH),
        y + bezierH.p0.y * radius,
      );
    }
    x = width - radius;
    y = 0;
    lineTo(x + offsetH, y);
    if (radius > 0) {
      cubicTo(
        x + bezierH.p1.x * radius,
        y + bezierH.p1.y * radius,
        x + bezierH.p2.x * radius,
        y + bezierH.p2.y * radius,
        x + bezierH.p3.x * radius,
        y + bezierH.p3.y * radius,
      );
      arcToScaled(
        _G2Point(width - radius, radius),
        radius,
        radiusScale,
        -math.pi * .5 + bezierArc,
        arcSweep,
      );
      x = width;
      y = radius;
      cubicTo(
        x - bezierV.p2.y * radius,
        y - bezierV.p2.x * radius,
        x - bezierV.p1.y * radius,
        y - bezierV.p1.x * radius,
        x - bezierV.p0.y * radius,
        y - math.max(bezierV.p0.x * radius, offsetV),
      );
    }
    x = width;
    y = height - radius;
    lineTo(x, y + offsetV);
    if (radius > 0) {
      cubicTo(
        x - bezierV.p1.y * radius,
        y + bezierV.p1.x * radius,
        x - bezierV.p2.y * radius,
        y + bezierV.p2.x * radius,
        x - bezierV.p3.y * radius,
        y + bezierV.p3.x * radius,
      );
      arcToScaled(
        _G2Point(width - radius, height - radius),
        radius,
        radiusScale,
        bezierArc,
        arcSweep,
      );
      x = width - radius;
      y = height;
      cubicTo(
        x + bezierH.p2.x * radius,
        y - bezierH.p2.y * radius,
        x + bezierH.p1.x * radius,
        y - bezierH.p1.y * radius,
        x + math.max(bezierH.p0.x * radius, offsetH),
        y - bezierH.p0.y * radius,
      );
    }
    x = radius;
    y = height;
    lineTo(x - offsetH, y);
    if (radius > 0) {
      cubicTo(
        x - bezierH.p1.x * radius,
        y - bezierH.p1.y * radius,
        x - bezierH.p2.x * radius,
        y - bezierH.p2.y * radius,
        x - bezierH.p3.x * radius,
        y - bezierH.p3.y * radius,
      );
      arcToScaled(
        _G2Point(radius, height - radius),
        radius,
        radiusScale,
        math.pi * .5 + bezierArc,
        arcSweep,
      );
      x = 0;
      y = height - radius;
      cubicTo(
        x + bezierV.p2.y * radius,
        y + bezierV.p2.x * radius,
        x + bezierV.p1.y * radius,
        y + bezierV.p1.x * radius,
        x + bezierV.p0.y * radius,
        y + math.max(bezierV.p0.x * radius, offsetV),
      );
    }
    path.close();
  }

  void appendHorizontalCapsule(double width, double height) {
    final radius = height * .5;
    final centerX = width * .5;
    const capsule = _G2Profile.capsule;
    const rounded = _G2Profile.roundedRectangle;
    final ratio = _cornerRatio(centerX, radius, capsule.extendedFraction);
    final offset = -radius * capsule.extendedFraction * ratio;
    final bezier = _G2Profile(
      capsule.extendedFraction * ratio,
      capsule.arcFraction,
      _lerp(capsule.bezierCurvatureScale, rounded.bezierCurvatureScale, ratio),
      1,
    ).bezier.scale(radius);
    final arc = math.pi * .5 * capsule.arcFraction;
    final bezierArc = (math.pi * .5 - arc) * .5;
    final sweep = (bezierArc + arc) * 2;
    var x = 0.0;
    var y = radius;
    moveTo(x, y);
    arcTo(_G2Point(radius, radius), radius, math.pi * .5 + bezierArc, sweep);
    x = radius;
    y = 0;
    cubicTo(
      x - bezier.p2.x,
      y + bezier.p2.y,
      x - bezier.p1.x,
      y + bezier.p1.y,
      x - math.max(bezier.p0.x, offset),
      y + bezier.p0.y,
    );
    x = width - radius;
    lineTo(x + offset, y);
    cubicTo(
      x + bezier.p1.x,
      y + bezier.p1.y,
      x + bezier.p2.x,
      y + bezier.p2.y,
      x + bezier.p3.x,
      y + bezier.p3.y,
    );
    arcTo(
      _G2Point(width - radius, radius),
      radius,
      -(math.pi * .5 - bezierArc),
      sweep,
    );
    x = width - radius;
    y = height;
    cubicTo(
      x + bezier.p2.x,
      y - bezier.p2.y,
      x + bezier.p1.x,
      y - bezier.p1.y,
      x + math.max(bezier.p0.x, offset),
      y - bezier.p0.y,
    );
    x = radius;
    lineTo(x - offset, y);
    cubicTo(
      x - bezier.p1.x,
      y - bezier.p1.y,
      x - bezier.p2.x,
      y - bezier.p2.y,
      x - bezier.p3.x,
      y - bezier.p3.y,
    );
    path.close();
  }

  void appendVerticalCapsule(double width, double height) {
    final radius = width * .5;
    final centerY = height * .5;
    const capsule = _G2Profile.capsule;
    const rounded = _G2Profile.roundedRectangle;
    final ratio = _cornerRatio(centerY, radius, capsule.extendedFraction);
    final offset = -radius * capsule.extendedFraction * ratio;
    final bezier = _G2Profile(
      capsule.extendedFraction * ratio,
      capsule.arcFraction,
      _lerp(capsule.bezierCurvatureScale, rounded.bezierCurvatureScale, ratio),
      1,
    ).bezier.scale(radius);
    final arc = math.pi * .5 * capsule.arcFraction;
    final bezierArc = (math.pi * .5 - arc) * .5;
    final sweep = (bezierArc + arc) * 2;
    var x = 0.0;
    var y = radius;
    moveTo(x, y - offset);
    cubicTo(
      x + bezier.p1.y,
      y - bezier.p1.x,
      x + bezier.p2.y,
      y - bezier.p2.x,
      x + bezier.p3.y,
      y - bezier.p3.x,
    );
    arcTo(_G2Point(radius, radius), radius, -(math.pi - bezierArc), sweep);
    x = width;
    y = radius;
    cubicTo(
      x - bezier.p2.y,
      y - bezier.p2.x,
      x - bezier.p1.y,
      y - bezier.p1.x,
      x - bezier.p0.y,
      y - math.max(bezier.p0.x, offset),
    );
    y = height - radius;
    lineTo(x, y + offset);
    cubicTo(
      x - bezier.p1.y,
      y + bezier.p1.x,
      x - bezier.p2.y,
      y + bezier.p2.x,
      x - bezier.p3.y,
      y + bezier.p3.x,
    );
    arcTo(_G2Point(width - radius, height - radius), radius, bezierArc, sweep);
    x = 0;
    y = height - radius;
    cubicTo(
      x + bezier.p2.y,
      y + bezier.p2.x,
      x + bezier.p1.y,
      y + bezier.p1.x,
      x + bezier.p0.y,
      y + math.max(bezier.p0.x, offset),
    );
    path.close();
  }
}

class _G2Profile {
  const _G2Profile(
    this.extendedFraction,
    this.arcFraction,
    this.bezierCurvatureScale,
    this.arcCurvatureScale,
  );
  final double extendedFraction;
  final double arcFraction;
  final double bezierCurvatureScale;
  final double arcCurvatureScale;
  static const roundedRectangle = _G2Profile(
    .5286651,
    5 / 9,
    1.0732051,
    1.0732051,
  );
  static const capsule = _G2Profile(.5286651 * .75, 0, 1, 1);
  _G2Bezier get bezier {
    final arcRadians = math.pi * .5 * arcFraction;
    final bezierRadians = (math.pi * .5 - arcRadians) * .5;
    final sine = math.sin(bezierRadians);
    final cosine = math.cos(bezierRadians);
    if (bezierCurvatureScale == 1 && arcCurvatureScale == 1) {
      final halfTan = sine / (1 + cosine);
      return _G2Bezier(
        _G2Point(-extendedFraction, 0),
        _G2Point((1 - 1.5 / (1 + cosine)) * halfTan, 0),
        _G2Point(halfTan, 0),
        _G2Point(sine, 1 - cosine),
      );
    }
    final radiusScale = 1 / arcCurvatureScale;
    final center =
        const _G2Point(0, 1) +
        const _G2Point(.7071067811865476, -.7071067811865476) *
            (1 - radiusScale);
    final arcStart = center + _G2Point(sine, -cosine) * radiusScale;
    return _bezierWithZeroStartCurvature(
      _G2Point(-extendedFraction, 0),
      arcStart,
      const _G2Point(1, 0),
      _G2Point(cosine, sine),
      bezierCurvatureScale,
    );
  }
}

class _G2Point {
  const _G2Point(this.x, this.y);
  final double x;
  final double y;
  _G2Point operator +(_G2Point other) => _G2Point(x + other.x, y + other.y);
  _G2Point operator -(_G2Point other) => _G2Point(x - other.x, y - other.y);
  _G2Point operator *(double scale) => _G2Point(x * scale, y * scale);
}

class _G2Bezier {
  const _G2Bezier(this.p0, this.p1, this.p2, this.p3);
  final _G2Point p0;
  final _G2Point p1;
  final _G2Point p2;
  final _G2Point p3;
  _G2Bezier scale(double value) =>
      _G2Bezier(p0 * value, p1 * value, p2 * value, p3 * value);
}

_G2Bezier _bezierWithZeroStartCurvature(
  _G2Point start,
  _G2Point end,
  _G2Point startTangent,
  _G2Point endTangent,
  double endCurvature,
) {
  final a2 = 1.5 * endCurvature;
  final b = startTangent.x * endTangent.y - startTangent.y * endTangent.x;
  final dx = end.x - start.x;
  final dy = end.y - start.y;
  final c1 = -dy * startTangent.x + dx * startTangent.y;
  final c2 = dy * endTangent.x - dx * endTangent.y;
  final lambda0 = -c2 / b - a2 * c1 * c1 / b / b / b;
  final lambda3 = -c1 / b;
  return _G2Bezier(
    start,
    start +
        _G2Point(
          math.max(lambda0 * startTangent.x, 0),
          math.max(lambda0 * startTangent.y, 0),
        ),
    end -
        _G2Point(
          math.max(lambda3 * endTangent.x, 0),
          math.max(lambda3 * endTangent.y, 0),
        ),
    end,
  );
}

double _cornerRatio(
  double halfDimension,
  double radius,
  double extendedFraction,
) {
  if (radius <= 0) return 1;
  return ((halfDimension / radius - 1) / extendedFraction).clamp(0, 1);
}

double _lerp(double start, double stop, double fraction) =>
    start + (stop - start) * fraction;
