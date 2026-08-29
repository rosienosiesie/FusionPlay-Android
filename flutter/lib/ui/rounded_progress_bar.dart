import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'g2_shape.dart';
import 'remote_focus.dart';

class RoundedProgressBar extends StatefulWidget {
  const RoundedProgressBar({
    super.key,
    required this.value,
    required this.enabled,
    required this.activeColor,
    required this.inactiveColor,
    required this.onChanged,
    this.seekEnabled,
    this.showThumb = true,
    this.focusBorderColor = Colors.black,
    this.focusNode,
    this.upFocusNode,
    this.downFocusNode,
    this.onChangeStart,
    this.onChangeEnd,
  });
  final double value;
  final bool enabled;
  final Color activeColor;
  final Color inactiveColor;
  final ValueChanged<double> onChanged;
  final bool? seekEnabled;
  final bool showThumb;
  final Color focusBorderColor;
  final FocusNode? focusNode;
  final FocusNode? upFocusNode;
  final FocusNode? downFocusNode;
  final ValueChanged<double>? onChangeStart;
  final ValueChanged<double>? onChangeEnd;

  @override
  State<RoundedProgressBar> createState() => _RoundedProgressBarState();
}

class _RoundedProgressBarState extends State<RoundedProgressBar> {
  double? dragging;
  bool focused = false;
  bool touching = false;

  bool get interactionEnabled => widget.enabled && (widget.seekEnabled ?? true);

  void setTouching(bool value) {
    if (touching != value) setState(() => touching = value);
  }

  @override
  void didUpdateWidget(covariant RoundedProgressBar oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (!interactionEnabled) touching = false;
  }

  void update(Offset local, double width) {
    if (!interactionEnabled || width <= 0) return;
    const thumbRadius = 8.0;
    final usableWidth = math.max(1.0, width - thumbRadius * 2);
    final next = ((local.dx - thumbRadius) / usableWidth)
        .clamp(0, 1)
        .toDouble();
    setState(() => dragging = next);
    widget.onChanged(next);
  }

  void adjust(double delta) {
    if (!interactionEnabled) return;
    final current = (dragging ?? widget.value).clamp(0.0, 1.0);
    final next = (current + delta).clamp(0.0, 1.0).toDouble();
    widget.onChangeStart?.call(current);
    widget.onChanged(next);
    widget.onChangeEnd?.call(next);
  }

  @override
  Widget build(BuildContext context) => LayoutBuilder(
    builder: (context, constraints) {
      final value = (dragging ?? widget.value).clamp(0.0, 1.0);
      final highlighted = focused || touching;
      final reduceMotion =
          MediaQuery.maybeOf(context)?.disableAnimations ?? false;
      return RemoteFocusFrame(
        enabled: interactionEnabled,
        focusNode: widget.focusNode,
        showBorder: false,
        onFocusChange: (value) {
          if (focused != value) setState(() => focused = value);
        },
        borderColor: widget.focusBorderColor,
        shape: const G2ContinuousBorder(radius: 20),
        shortcuts: <ShortcutActivator, Intent>{
          const SingleActivator(LogicalKeyboardKey.arrowLeft):
              const RemoteAdjustIntent(-.02),
          const SingleActivator(LogicalKeyboardKey.arrowRight):
              const RemoteAdjustIntent(.02),
          if (widget.upFocusNode != null)
            const SingleActivator(LogicalKeyboardKey.arrowUp):
                const DirectionalFocusIntent(TraversalDirection.up),
          if (widget.downFocusNode != null)
            const SingleActivator(LogicalKeyboardKey.arrowDown):
                const DirectionalFocusIntent(TraversalDirection.down),
        },
        actions: <Type, Action<Intent>>{
          RemoteAdjustIntent: CallbackAction<RemoteAdjustIntent>(
            onInvoke: (intent) {
              adjust(intent.delta);
              return null;
            },
          ),
          if (widget.upFocusNode != null || widget.downFocusNode != null)
            DirectionalFocusIntent: CallbackAction<DirectionalFocusIntent>(
              onInvoke: (intent) {
                if (intent.direction == TraversalDirection.up) {
                  widget.upFocusNode?.requestFocus();
                } else if (intent.direction == TraversalDirection.down) {
                  widget.downFocusNode?.requestFocus();
                }
                return null;
              },
            ),
        },
        child: TweenAnimationBuilder<double>(
          duration: reduceMotion
              ? Duration.zero
              : const Duration(milliseconds: 100),
          curve: Curves.easeOutCubic,
          tween: Tween(end: highlighted ? 1 : 0),
          builder: (context, focusProgress, _) => Listener(
            onPointerDown: interactionEnabled ? (_) => setTouching(true) : null,
            onPointerUp: interactionEnabled ? (_) => setTouching(false) : null,
            onPointerCancel: interactionEnabled
                ? (_) => setTouching(false)
                : null,
            child: GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTapDown: interactionEnabled
                  ? (e) {
                      update(e.localPosition, constraints.maxWidth);
                      widget.onChangeStart?.call(dragging ?? value);
                    }
                  : null,
              onTapUp: interactionEnabled
                  ? (_) {
                      widget.onChangeEnd?.call(dragging ?? value);
                      setState(() => dragging = null);
                    }
                  : null,
              onHorizontalDragStart: interactionEnabled
                  ? (e) {
                      update(e.localPosition, constraints.maxWidth);
                      widget.onChangeStart?.call(dragging ?? value);
                    }
                  : null,
              onHorizontalDragUpdate: interactionEnabled
                  ? (e) => update(e.localPosition, constraints.maxWidth)
                  : null,
              onHorizontalDragEnd: interactionEnabled
                  ? (_) {
                      widget.onChangeEnd?.call(dragging ?? value);
                      setState(() => dragging = null);
                    }
                  : null,
              child: SizedBox(
                height: 40,
                child: CustomPaint(
                  painter: _ProgressPainter(
                    value: value,
                    active: widget.enabled
                        ? widget.activeColor
                        : widget.activeColor.withValues(
                            alpha: widget.activeColor.a * .38,
                          ),
                    inactive: widget.enabled
                        ? widget.inactiveColor
                        : widget.inactiveColor.withValues(
                            alpha: widget.inactiveColor.a * .5,
                          ),
                    showThumb: widget.showThumb,
                    focusProgress: focusProgress,
                  ),
                ),
              ),
            ),
          ),
        ),
      );
    },
  );
}

class _ProgressPainter extends CustomPainter {
  const _ProgressPainter({
    required this.value,
    required this.active,
    required this.inactive,
    required this.showThumb,
    required this.focusProgress,
  });
  final double value;
  final Color active;
  final Color inactive;
  final bool showThumb;
  final double focusProgress;

  @override
  void paint(Canvas canvas, Size size) {
    final y = size.height / 2;
    final thumbRadius = 8 + 2 * focusProgress;
    final trackStart = math.min(thumbRadius, size.width / 2);
    final trackEnd = math.max(trackStart, size.width - thumbRadius);
    final trackWidth = trackEnd - trackStart;
    final trackHeight = 6 + 2 * focusProgress;
    final inactivePaint = Paint()
      ..color = Color.lerp(inactive, active, .24 * focusProgress)!
      ..strokeWidth = trackHeight
      ..strokeCap = StrokeCap.round;
    canvas.drawLine(Offset(trackStart, y), Offset(trackEnd, y), inactivePaint);
    final activeWidth = math.max(0.0, trackWidth * value);
    if (activeWidth > 0) {
      canvas.drawLine(
        Offset(trackStart, y),
        Offset(trackStart + activeWidth, y),
        Paint()
          ..color = active
          ..strokeWidth = trackHeight
          ..strokeCap = StrokeCap.round,
      );
    }
    if (showThumb) {
      canvas.drawCircle(
        Offset(trackStart + activeWidth, y),
        thumbRadius,
        Paint()..color = active,
      );
    }
  }

  @override
  bool shouldRepaint(_ProgressPainter old) =>
      old.value != value ||
      old.active != active ||
      old.inactive != inactive ||
      old.showThumb != showThumb ||
      old.focusProgress != focusProgress;
}
