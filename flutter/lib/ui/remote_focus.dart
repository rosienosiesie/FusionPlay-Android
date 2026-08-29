import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'g2_shape.dart';

class RemoteFocusFrame extends StatefulWidget {
  const RemoteFocusFrame({
    super.key,
    required this.child,
    this.enabled = true,
    this.autofocus = false,
    this.focusNode,
    this.onActivate,
    this.onPressChange,
    this.onFocusChange,
    this.borderColor = Colors.black,
    this.shape = const G2ContinuousBorder(radius: 20),
    this.showBorder = true,
    this.shortcuts = const <ShortcutActivator, Intent>{},
    this.actions = const <Type, Action<Intent>>{},
  });

  final Widget child;
  final bool enabled;
  final bool autofocus;
  final FocusNode? focusNode;
  final VoidCallback? onActivate;
  final ValueChanged<bool>? onPressChange;
  final ValueChanged<bool>? onFocusChange;
  final Color borderColor;
  final OutlinedBorder shape;
  final bool showBorder;
  final Map<ShortcutActivator, Intent> shortcuts;
  final Map<Type, Action<Intent>> actions;

  @override
  State<RemoteFocusFrame> createState() => _RemoteFocusFrameState();
}

class _RemoteFocusFrameState extends State<RemoteFocusFrame> {
  bool _showFocus = false;
  bool _focused = false;
  bool _activationPressed = false;

  @override
  void initState() {
    super.initState();
    HardwareKeyboard.instance.addHandler(_handleHardwareKey);
  }

  @override
  void dispose() {
    HardwareKeyboard.instance.removeHandler(_handleHardwareKey);
    super.dispose();
  }

  @override
  void didUpdateWidget(covariant RemoteFocusFrame oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.enabled && !widget.enabled) _setActivationPressed(false);
  }

  bool _isActivationKey(LogicalKeyboardKey key) =>
      key == LogicalKeyboardKey.select ||
      key == LogicalKeyboardKey.enter ||
      key == LogicalKeyboardKey.space;

  bool _handleHardwareKey(KeyEvent event) {
    if (!_focused || !_isActivationKey(event.logicalKey)) {
      return false;
    }
    if (event is KeyDownEvent && widget.enabled) {
      _setActivationPressed(true);
    } else if (event is KeyUpEvent) {
      _setActivationPressed(false);
    }
    return false;
  }

  void _setActivationPressed(bool pressed) {
    if (_activationPressed == pressed) return;
    _activationPressed = pressed;
    widget.onPressChange?.call(pressed);
  }

  @override
  Widget build(BuildContext context) {
    final shortcuts = <ShortcutActivator, Intent>{
      const SingleActivator(LogicalKeyboardKey.select): const ActivateIntent(),
      const SingleActivator(LogicalKeyboardKey.enter): const ActivateIntent(),
      const SingleActivator(LogicalKeyboardKey.space): const ActivateIntent(),
      ...widget.shortcuts,
    };
    final actions = <Type, Action<Intent>>{
      if (widget.onActivate != null)
        ActivateIntent: CallbackAction<ActivateIntent>(
          onInvoke: (_) {
            widget.onActivate!();
            return null;
          },
        ),
      ...widget.actions,
    };

    return FocusableActionDetector(
      enabled: widget.enabled,
      autofocus: widget.autofocus,
      focusNode: widget.focusNode,
      descendantsAreFocusable: false,
      descendantsAreTraversable: false,
      shortcuts: shortcuts,
      actions: actions,
      onShowFocusHighlight: (show) {
        if (_showFocus != show) setState(() => _showFocus = show);
      },
      onFocusChange: (focused) {
        _focused = focused;
        if (!focused) _setActivationPressed(false);
        widget.onFocusChange?.call(focused);
        if (!focused && _showFocus) setState(() => _showFocus = false);
        if (!focused) return;
        WidgetsBinding.instance.addPostFrameCallback((_) {
          if (!mounted) return;
          final reduceMotion =
              MediaQuery.maybeOf(context)?.disableAnimations ?? false;
          Scrollable.ensureVisible(
            context,
            duration: reduceMotion
                ? Duration.zero
                : const Duration(milliseconds: 120),
            curve: Curves.easeOutCubic,
            alignment: .5,
          );
        });
      },
      child: AnimatedContainer(
        duration: MediaQuery.maybeOf(context)?.disableAnimations ?? false
            ? Duration.zero
            : const Duration(milliseconds: 100),
        curve: Curves.easeOut,
        foregroundDecoration: ShapeDecoration(
          shape: widget.shape.copyWith(
            side: widget.showBorder
                ? BorderSide(
                    color: widget.borderColor.withValues(
                      alpha: _showFocus ? widget.borderColor.a : 0,
                    ),
                    width: 3,
                  )
                : BorderSide.none,
          ),
        ),
        child: widget.child,
      ),
    );
  }
}

class RemoteAdjustIntent extends Intent {
  const RemoteAdjustIntent(this.delta);

  final double delta;
}
