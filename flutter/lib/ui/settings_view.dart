import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_svg/flutter_svg.dart';

import '../models/app_state.dart';
import '../theme/fusion_theme.dart';
import 'g2_shape.dart';
import 'remote_focus.dart';
import 'vector_icon.dart';

const fusionPlayVersion = String.fromEnvironment(
  'FUSIONPLAY_VERSION',
  defaultValue: '1.2.4',
);

class SettingsView extends StatefulWidget {
  const SettingsView({
    super.key,
    required this.state,
    required this.initialFocusNode,
    required this.onClose,
    required this.onReceiverName,
    required this.onStartup,
    required this.onAutoWake,
    required this.onAdvancedEffects,
    required this.onMiPlay,
    required this.onMiPlayIdentity,
    required this.onAirPlay,
    required this.onDlna,
    required this.onExportLogs,
  });

  final AppState state;
  final FocusNode initialFocusNode;
  final VoidCallback onClose;
  final ValueChanged<String> onReceiverName;
  final ValueChanged<bool> onStartup;
  final ValueChanged<bool> onAutoWake;
  final ValueChanged<bool> onAdvancedEffects;
  final ValueChanged<bool> onMiPlay;
  final ValueChanged<MiPlayDeviceIdentity> onMiPlayIdentity;
  final ValueChanged<bool> onAirPlay;
  final ValueChanged<bool> onDlna;
  final Future<void> Function() onExportLogs;

  @override
  State<SettingsView> createState() => _SettingsViewState();
}

class _SettingsViewState extends State<SettingsView> {
  late final TextEditingController _name;
  late final FocusNode _nameFocus;
  late String _savedName;
  bool _exportingLogs = false;

  @override
  void initState() {
    super.initState();
    _savedName = widget.state.settings.receiverName ?? '';
    _nameFocus = FocusNode(debugLabel: 'receiver-name');
    _name = TextEditingController(text: _savedName)..addListener(_refresh);
  }

  @override
  void didUpdateWidget(covariant SettingsView oldWidget) {
    super.didUpdateWidget(oldWidget);
    final incoming = widget.state.settings.receiverName ?? '';
    if (incoming != _savedName && _name.text == _savedName) {
      _savedName = incoming;
      _name.text = incoming;
    }
  }

  void _refresh() => setState(() {});

  @override
  void dispose() {
    _name.removeListener(_refresh);
    _name.dispose();
    _nameFocus.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final settings = widget.state.settings;
    final padding = MediaQuery.paddingOf(context);
    return ColoredBox(
      color: Colors.white,
      child: FocusTraversalGroup(
        policy: ReadingOrderTraversalPolicy(),
        child: SingleChildScrollView(
          padding: EdgeInsets.fromLTRB(28, padding.top + 24, 28, 0),
          child: Column(
            children: [
              const SizedBox(
                height: 56,
                child: Padding(
                  padding: EdgeInsets.only(bottom: 4),
                  child: Center(
                    child: Text(
                      '设置',
                      maxLines: 1,
                      style: TextStyle(
                        fontSize: 28,
                        fontWeight: FusionTypography.boldFontWeight,
                        fontVariations: FusionTypography.bold,
                      ),
                    ),
                  ),
                ),
              ),
              const SizedBox(height: SettingsStyle.cardSpacing),
              _card([_nameEditor()]),
              const SizedBox(height: SettingsStyle.cardSpacing),
              _card([
                _switchRow(
                  asset: 'assets/miplay-logo.svg',
                  preserveAssetColors: true,
                  title: 'MiPlay',
                  value: settings.miPlayEnabled,
                  focusNode: widget.initialFocusNode,
                  focusNameOnUp: true,
                  onChanged: widget.onMiPlay,
                ),
                _switchRow(
                  asset: 'assets/airplay-broadcast.svg',
                  title: 'AirPlay',
                  value: settings.airPlayEnabled,
                  onChanged: widget.onAirPlay,
                ),
                _switchRow(
                  asset: FusionIcons.dlna,
                  title: 'DLNA',
                  value: settings.dlnaEnabled,
                  onChanged: widget.onDlna,
                ),
              ]),
              const SizedBox(height: SettingsStyle.cardSpacing),
              _card([
                for (final identity in MiPlayDeviceIdentity.values)
                  _identityRow(identity, settings.miPlayDeviceIdentity),
              ]),
              const SizedBox(height: SettingsStyle.cardSpacing),
              _card([
                _switchRow(
                  asset: 'assets/icon-power-settings-new.svg',
                  title: '开机自启',
                  value: settings.startupEnabled,
                  onChanged: widget.onStartup,
                ),
                _switchRow(
                  asset: 'assets/auto-wake.svg',
                  title: '自动唤起',
                  value: settings.autoWakeEnabled,
                  onChanged: widget.onAutoWake,
                ),
                _switchRow(
                  icon: Icons.auto_awesome_rounded,
                  title: '高级效果',
                  value: settings.advancedEffectsEnabled,
                  onChanged: widget.onAdvancedEffects,
                ),
              ]),
              const SizedBox(height: SettingsStyle.cardSpacing),
              _card([_aboutRow(), _exportLogsRow()]),
            ],
          ),
        ),
      ),
    );
  }

  Widget _aboutRow() => const Padding(
    padding: EdgeInsets.fromLTRB(20, 16, 28.5, 16),
    child: Row(
      children: [
        SizedBox(
          width: 42,
          height: 42,
          child: Center(
            child: FusionVectorIcon(
              FusionIcons.about,
              size: 25,
              color: SettingsStyle.iconColor,
            ),
          ),
        ),
        SizedBox(width: 14),
        Expanded(
          child: Text(
            '关于应用',
            style: TextStyle(
              fontSize: 16,
              fontWeight: FusionTypography.semiBoldFontWeight,
              fontVariations: FusionTypography.semiBold,
            ),
          ),
        ),
        Text(
          fusionPlayVersion,
          style: TextStyle(color: SettingsStyle.secondaryText),
        ),
      ],
    ),
  );

  Widget _exportLogsRow() {
    final enabled = !widget.state.busy && !_exportingLogs;
    void export() {
      if (enabled) _exportLogs();
    }

    return RemoteFocusFrame(
      enabled: enabled,
      onActivate: export,
      shape: const G2ContinuousBorder(radius: 20),
      child: InkWell(
        key: const ValueKey('export-logs'),
        onTap: enabled ? export : null,
        child: Padding(
          padding: const EdgeInsets.fromLTRB(20, 16, 28.5, 16),
          child: Row(
            children: [
              const SizedBox(
                width: 42,
                height: 42,
                child: Center(
                  child: FusionVectorIcon(
                    FusionIcons.exportLogs,
                    size: 25,
                    color: SettingsStyle.iconColor,
                  ),
                ),
              ),
              const SizedBox(width: 14),
              const Expanded(
                child: Text(
                  '导出日志',
                  style: TextStyle(
                    fontSize: 16,
                    fontWeight: FusionTypography.semiBoldFontWeight,
                    fontVariations: FusionTypography.semiBold,
                  ),
                ),
              ),
              if (_exportingLogs)
                const SizedBox.square(
                  dimension: 20,
                  child: CircularProgressIndicator(
                    strokeWidth: 2.5,
                    color: SettingsStyle.iconColor,
                  ),
                )
              else
                const Icon(
                  Icons.ios_share_rounded,
                  size: 22,
                  color: SettingsStyle.iconColor,
                ),
            ],
          ),
        ),
      ),
    );
  }

  Future<void> _exportLogs() async {
    if (_exportingLogs) return;
    setState(() => _exportingLogs = true);
    try {
      await widget.onExportLogs();
    } finally {
      if (mounted) setState(() => _exportingLogs = false);
    }
  }

  Widget _nameEditor() {
    final dirty = _name.text.trim() != _savedName.trim();
    final restoreEnabled =
        !widget.state.busy && (_savedName.isNotEmpty || dirty);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const _SectionTitle(asset: FusionIcons.customName, text: '自定义名称'),
          const SizedBox(height: 12),
          Focus(
            canRequestFocus: false,
            skipTraversal: true,
            onKeyEvent: (_, event) {
              if (event is KeyDownEvent &&
                  event.logicalKey == LogicalKeyboardKey.arrowDown) {
                _nameFocus.nextFocus();
                return KeyEventResult.handled;
              }
              return KeyEventResult.ignored;
            },
            child: TextField(
              focusNode: _nameFocus,
              controller: _name,
              maxLength: 63,
              textInputAction: TextInputAction.done,
              decoration: InputDecoration(
                counterText: '',
                hintText: widget.state.receiverName,
                hintStyle: const TextStyle(color: SettingsStyle.secondaryText),
                filled: true,
                fillColor: Colors.white,
                border: const OutlineInputBorder(
                  borderRadius: BorderRadius.all(Radius.circular(28)),
                  borderSide: BorderSide.none,
                ),
                enabledBorder: const OutlineInputBorder(
                  borderRadius: BorderRadius.all(Radius.circular(28)),
                  borderSide: BorderSide.none,
                ),
                focusedBorder: const OutlineInputBorder(
                  borderRadius: BorderRadius.all(Radius.circular(28)),
                  borderSide: BorderSide(color: Colors.black, width: 3),
                ),
                contentPadding: const EdgeInsets.symmetric(
                  horizontal: 20,
                  vertical: 16,
                ),
              ),
            ),
          ),
          const SizedBox(height: 12),
          Row(
            children: [
              Expanded(
                child: _actionButton(
                  label: '恢复默认',
                  active: restoreEnabled,
                  dark: false,
                  onPressed: restoreEnabled
                      ? () {
                          _name.clear();
                          widget.onReceiverName('');
                          setState(() => _savedName = '');
                        }
                      : null,
                ),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: _actionButton(
                  label: '应用',
                  active: !widget.state.busy && dirty,
                  dark: true,
                  onPressed: !widget.state.busy && dirty
                      ? () {
                          final value = _name.text.trim();
                          widget.onReceiverName(value);
                          setState(() => _savedName = value);
                        }
                      : null,
                ),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _actionButton({
    required String label,
    required bool active,
    required bool dark,
    required VoidCallback? onPressed,
  }) => RemoteFocusFrame(
    enabled: active,
    onActivate: onPressed,
    shape: const G2ContinuousBorder(radius: 25),
    child: AnimatedContainer(
      height: 50,
      duration: MediaQuery.maybeOf(context)?.disableAnimations ?? false
          ? Duration.zero
          : SettingsStyle.actionColorDuration,
      decoration: ShapeDecoration(
        color: active
            ? (dark ? Colors.black : Colors.white)
            : SettingsStyle.inactiveAction,
        shape: const G2ContinuousBorder(radius: 25),
      ),
      child: Material(
        color: Colors.transparent,
        shape: const G2ContinuousBorder(radius: 25),
        clipBehavior: Clip.antiAlias,
        child: InkWell(
          onTap: onPressed,
          child: Center(
            child: Text(
              label,
              style: TextStyle(
                color: active
                    ? (dark ? Colors.white : Colors.black)
                    : SettingsStyle.secondaryText,
                fontWeight: FusionTypography.mediumFontWeight,
                fontVariations: FusionTypography.medium,
              ),
            ),
          ),
        ),
      ),
    ),
  );

  Widget _identityRow(
    MiPlayDeviceIdentity identity,
    MiPlayDeviceIdentity selected,
  ) => RemoteFocusFrame(
    enabled: !widget.state.busy,
    onActivate: () => widget.onMiPlayIdentity(identity),
    shape: const G2ContinuousBorder(radius: 20),
    child: InkWell(
      onTap: widget.state.busy ? null : () => widget.onMiPlayIdentity(identity),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 16),
        child: Row(
          children: [
            SizedBox(
              width: 42,
              height: 42,
              child: Center(child: _identityIcon(identity)),
            ),
            const SizedBox(width: 14),
            Expanded(
              child: Text(
                identity.label,
                style: const TextStyle(
                  fontSize: 16,
                  fontWeight: FusionTypography.semiBoldFontWeight,
                  fontVariations: FusionTypography.semiBold,
                ),
              ),
            ),
            ExcludeFocus(
              child: SizedBox(
                width: 48,
                height: 48,
                child: Radio<MiPlayDeviceIdentity>(
                  key: MediaQuery.maybeOf(context)?.disableAnimations ?? false
                      ? ValueKey('identity-${identity.name}-${selected.name}')
                      : ValueKey('identity-${identity.name}'),
                  value: identity,
                  groupValue: selected,
                  activeColor: SettingsStyle.switchOnTrack,
                  fillColor: WidgetStateProperty.resolveWith(
                    (states) => states.contains(WidgetState.selected)
                        ? SettingsStyle.switchOnTrack
                        : Colors.black,
                  ),
                  onChanged: widget.state.busy
                      ? null
                      : (value) {
                          if (value != null) widget.onMiPlayIdentity(value);
                        },
                ),
              ),
            ),
          ],
        ),
      ),
    ),
  );

  Widget _identityIcon(MiPlayDeviceIdentity identity) => switch (identity) {
    MiPlayDeviceIdentity.television => SvgPicture.asset(
      'assets/device-television.svg',
      width: 24,
      height: 24,
    ),
    MiPlayDeviceIdentity.speaker => SvgPicture.asset(
      'assets/device-speaker.svg',
      width: 24,
      height: 24,
    ),
    MiPlayDeviceIdentity.vehicle => SvgPicture.asset(
      'assets/device-vehicle.svg',
      width: 24,
      height: 24,
      colorFilter: const ColorFilter.mode(
        SettingsStyle.iconColor,
        BlendMode.srcIn,
      ),
    ),
    MiPlayDeviceIdentity.tablet => SvgPicture.asset(
      'assets/device-tablet.svg',
      width: 24,
      height: 24,
      colorFilter: const ColorFilter.mode(
        SettingsStyle.iconColor,
        BlendMode.srcIn,
      ),
    ),
  };

  Widget _switchRow({
    String? asset,
    IconData? icon,
    bool preserveAssetColors = false,
    FocusNode? focusNode,
    bool focusNameOnUp = false,
    required String title,
    required bool value,
    required ValueChanged<bool> onChanged,
  }) => RemoteFocusFrame(
    enabled: !widget.state.busy,
    focusNode: focusNode,
    onActivate: () => onChanged(!value),
    shortcuts: focusNameOnUp
        ? const <ShortcutActivator, Intent>{
            SingleActivator(LogicalKeyboardKey.arrowUp):
                _FocusReceiverNameIntent(),
          }
        : const <ShortcutActivator, Intent>{},
    actions: focusNameOnUp
        ? <Type, Action<Intent>>{
            _FocusReceiverNameIntent: CallbackAction<_FocusReceiverNameIntent>(
              onInvoke: (_) {
                _nameFocus.requestFocus();
                return null;
              },
            ),
          }
        : const <Type, Action<Intent>>{},
    shape: const G2ContinuousBorder(radius: 20),
    child: InkWell(
      onTap: widget.state.busy ? null : () => onChanged(!value),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 16),
        child: Row(
          children: [
            SizedBox(
              width: 42,
              height: 42,
              child: Center(
                child: asset != null
                    ? SvgPicture.asset(
                        asset,
                        width: 25,
                        height: 25,
                        colorFilter: preserveAssetColors
                            ? null
                            : const ColorFilter.mode(
                                SettingsStyle.iconColor,
                                BlendMode.srcIn,
                              ),
                      )
                    : Icon(icon, size: 25, color: SettingsStyle.iconColor),
              ),
            ),
            const SizedBox(width: 14),
            Expanded(
              child: Text(
                title,
                style: const TextStyle(
                  fontSize: 16,
                  fontWeight: FusionTypography.semiBoldFontWeight,
                  fontVariations: FusionTypography.semiBold,
                ),
              ),
            ),
            ExcludeFocus(
              child: SizedBox(
                width: SettingsStyle.switchWidth,
                height: SettingsStyle.switchHeight,
                child: _G2SettingsSwitch(
                  value: value,
                  onChanged: widget.state.busy ? null : onChanged,
                ),
              ),
            ),
          ],
        ),
      ),
    ),
  );

  Widget _card(List<Widget> children) => DecoratedBox(
    decoration: const ShapeDecoration(
      color: SettingsStyle.groupBackground,
      shape: G2ContinuousBorder(radius: SettingsStyle.cardRadius),
    ),
    child: Column(children: children),
  );
}

class _G2SettingsSwitch extends StatelessWidget {
  const _G2SettingsSwitch({required this.value, required this.onChanged});

  final bool value;
  final ValueChanged<bool>? onChanged;

  @override
  Widget build(BuildContext context) {
    final reduceMotion =
        MediaQuery.maybeOf(context)?.disableAnimations ?? false;
    return Semantics(
      button: true,
      enabled: onChanged != null,
      toggled: value,
      onTap: onChanged == null ? null : () => onChanged!(!value),
      child: GestureDetector(
        behavior: HitTestBehavior.opaque,
        onTap: onChanged == null ? null : () => onChanged!(!value),
        child: Opacity(
          opacity: onChanged == null ? .38 : 1,
          child: TweenAnimationBuilder<double>(
            tween: Tween(end: value ? 1 : 0),
            duration: reduceMotion ? Duration.zero : FusionMotion.stateChange,
            curve: FusionMotion.easeOut,
            builder: (context, progress, _) {
              final trackColor = Color.lerp(
                SettingsStyle.switchOffTrack,
                SettingsStyle.switchOnTrack,
                progress,
              )!;
              final thumbColor = Color.lerp(
                SettingsStyle.switchOffThumb,
                SettingsStyle.switchOnThumb,
                progress,
              )!;
              return DecoratedBox(
                decoration: ShapeDecoration(
                  color: trackColor,
                  shape: const G2ContinuousBorder(
                    radius: SettingsStyle.switchHeight / 2,
                  ),
                ),
                child: Stack(
                  children: [
                    Positioned(
                      left: 4 + 20 * progress,
                      top: 4,
                      child: DecoratedBox(
                        decoration: ShapeDecoration(
                          color: thumbColor,
                          shape: const CircleBorder(),
                        ),
                        child: const SizedBox.square(dimension: 24),
                      ),
                    ),
                  ],
                ),
              );
            },
          ),
        ),
      ),
    );
  }
}

class _FocusReceiverNameIntent extends Intent {
  const _FocusReceiverNameIntent();
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle({required this.asset, required this.text});
  final String asset;
  final String text;

  @override
  Widget build(BuildContext context) => Row(
    children: [
      SizedBox(
        width: 42,
        height: 42,
        child: Center(
          child: FusionVectorIcon(
            asset,
            size: 25,
            color: SettingsStyle.iconColor,
          ),
        ),
      ),
      const SizedBox(width: 14),
      Text(
        text,
        style: const TextStyle(
          fontSize: 16,
          fontWeight: FusionTypography.semiBoldFontWeight,
          fontVariations: FusionTypography.semiBold,
        ),
      ),
    ],
  );
}
