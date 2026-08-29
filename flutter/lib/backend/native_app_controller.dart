import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../models/app_state.dart';

class NativeAppController extends ChangeNotifier {
  static const _methods = MethodChannel('com.fusionplay.android/runtime');
  static const _events = EventChannel('com.fusionplay.android/runtime_events');

  AppState state = const AppState();
  StreamSubscription<Object?>? _subscription;

  Future<void> initialize() async {
    try {
      _subscription = _events.receiveBroadcastStream().listen(
        _acceptState,
        onError: (Object error) => _acceptError(error),
      );
      _acceptState(await _methods.invokeMethod<Object?>('state'));
    } catch (error) {
      _acceptError(error);
    }
  }

  void _acceptState(Object? raw) {
    if (raw is! Map) return;
    final next = AppState.fromMap(raw.cast<Object?, Object?>());
    if (next == state) return;
    state = next;
    notifyListeners();
  }

  Future<void> setReceiverName(String value) =>
      _invoke('setReceiverName', {'value': value});

  Future<void> setStartupEnabled(bool value) =>
      _invoke('setStartupEnabled', {'value': value});

  Future<void> setAutoWakeEnabled(bool value) =>
      _invoke('setAutoWakeEnabled', {'value': value});

  Future<void> setAdvancedEffectsEnabled(bool value) =>
      _invoke('setAdvancedEffectsEnabled', {'value': value});

  Future<void> setProtocolEnabled(String protocol, bool value) =>
      _invoke('setProtocolEnabled', {'protocol': protocol, 'value': value});

  Future<void> setMiPlayDeviceIdentity(MiPlayDeviceIdentity identity) =>
      _invoke('setMiPlayDeviceIdentity', {'value': identity.persistedValue});

  Future<void> playback(PlaybackCommand command) =>
      _invoke('playback', {'command': command.name});

  Future<void> seek(int positionMs) =>
      _invoke('seek', {'positionMs': positionMs});

  Future<void> volume(int percent) => _invoke('volume', {'percent': percent});

  Future<void> exportLogs() => _invoke('exportLogs');

  Future<void> clearError() => _invoke('clearError');

  Future<void> _invoke(String method, [Map<String, Object?>? arguments]) async {
    try {
      final response = await _methods.invokeMethod<Object?>(method, arguments);
      _acceptState(response);
    } catch (error) {
      _acceptError(error);
    }
  }

  void _acceptError(Object error) {
    final next = state.copyWith(lastError: error.toString());
    if (next == state) return;
    state = next;
    notifyListeners();
  }

  @override
  void dispose() {
    _subscription?.cancel();
    super.dispose();
  }
}
