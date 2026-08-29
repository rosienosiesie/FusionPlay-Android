import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../theme/fusion_theme.dart';

/// Pointer-only press feedback for controls that retain their own semantics,
/// focus handling, and activation behavior in [child].
class AnimatedPressScale extends StatefulWidget {
  const AnimatedPressScale({
    super.key,
    required this.child,
    this.enabled = true,
    this.externalPressed,
  });

  final Widget child;
  final bool enabled;
  final ValueListenable<bool>? externalPressed;

  @override
  State<AnimatedPressScale> createState() => _AnimatedPressScaleState();
}

class _AnimatedPressScaleState extends State<AnimatedPressScale> {
  bool pointerPressed = false;

  bool get pressed =>
      pointerPressed || (widget.externalPressed?.value ?? false);

  @override
  void initState() {
    super.initState();
    widget.externalPressed?.addListener(_externalPressChanged);
  }

  @override
  void didUpdateWidget(covariant AnimatedPressScale oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.externalPressed != widget.externalPressed) {
      oldWidget.externalPressed?.removeListener(_externalPressChanged);
      widget.externalPressed?.addListener(_externalPressChanged);
    }
    if (!widget.enabled && pointerPressed) pointerPressed = false;
  }

  @override
  void dispose() {
    widget.externalPressed?.removeListener(_externalPressChanged);
    super.dispose();
  }

  void _externalPressChanged() {
    if (mounted) setState(() {});
  }

  void setPressed(bool value) {
    if (!widget.enabled || pointerPressed == value) return;
    setState(() => pointerPressed = value);
  }

  @override
  Widget build(BuildContext context) {
    final reduceMotion =
        MediaQuery.maybeOf(context)?.disableAnimations ?? false;
    return Listener(
      onPointerDown: widget.enabled ? (_) => setPressed(true) : null,
      onPointerUp: widget.enabled ? (_) => setPressed(false) : null,
      onPointerCancel: widget.enabled ? (_) => setPressed(false) : null,
      child: AnimatedScale(
        scale: pressed && !reduceMotion ? .97 : 1,
        duration: pressed
            ? FusionMotion.buttonPress
            : FusionMotion.buttonRelease,
        curve: FusionMotion.easeOut,
        child: widget.child,
      ),
    );
  }
}
