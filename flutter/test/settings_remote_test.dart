import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/backend/native_app_controller.dart';
import 'package:fusionplay_android_flutter/main.dart';
import 'package:fusionplay_android_flutter/models/app_state.dart';
import 'package:fusionplay_android_flutter/ui/settings_view.dart';

void main() {
  testWidgets('menu toggles settings and remote back closes it', (
    tester,
  ) async {
    final controller = NativeAppController();
    addTearDown(controller.dispose);
    tester.view.physicalSize = const Size(1200, 600);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        home: FusionPlayHome(
          controller: controller,
          state: const AppState(initialized: true),
        ),
      ),
    );
    await tester.pump();

    AnimatedPositioned panel() =>
        tester.widget<AnimatedPositioned>(find.byType(AnimatedPositioned));

    expect(panel().right, lessThan(0));
    expect(panel().duration, Duration.zero);
    await tester.sendKeyEvent(LogicalKeyboardKey.contextMenu);
    await tester.pumpAndSettle();
    expect(panel().right, 0);

    await tester.sendKeyEvent(LogicalKeyboardKey.contextMenu);
    await tester.pumpAndSettle();
    expect(panel().right, lessThan(0));

    await tester.sendKeyEvent(LogicalKeyboardKey.contextMenu);
    await tester.pumpAndSettle();
    expect(panel().right, 0);

    await tester.sendKeyEvent(LogicalKeyboardKey.escape);
    await tester.pumpAndSettle();
    expect(panel().right, lessThan(0));
  });

  testWidgets('up from protocol switches focuses the receiver name field', (
    tester,
  ) async {
    final initial = FocusNode(debugLabel: 'settings-miplay-test');
    addTearDown(initial.dispose);
    tester.view.physicalSize = const Size(480, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        theme: ThemeData(splashFactory: NoSplash.splashFactory),
        home: Scaffold(
          body: SettingsView(
            state: const AppState(initialized: true),
            initialFocusNode: initial,
            onClose: () {},
            onReceiverName: (_) {},
            onStartup: (_) {},
            onAutoWake: (_) {},
            onAdvancedEffects: (_) {},
            onMiPlay: (_) {},
            onMiPlayIdentity: (_) {},
            onAirPlay: (_) {},
            onDlna: (_) {},
            onExportLogs: () async {},
          ),
        ),
      ),
    );
    initial.requestFocus();
    await tester.pump();
    expect(initial.hasPrimaryFocus, isTrue);

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    await tester.pump();

    final textField = tester.widget<TextField>(find.byType(TextField));
    expect(textField.focusNode?.hasPrimaryFocus, isTrue);
  });

  testWidgets('日志导出入口与关于应用位于同一卡片并可触发', (tester) async {
    var exportCount = 0;
    final initial = FocusNode(debugLabel: 'settings-export-test');
    addTearDown(initial.dispose);
    tester.view.physicalSize = const Size(480, 1100);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        theme: ThemeData(splashFactory: NoSplash.splashFactory),
        home: Scaffold(
          body: SettingsView(
            state: const AppState(initialized: true),
            initialFocusNode: initial,
            onClose: () {},
            onReceiverName: (_) {},
            onStartup: (_) {},
            onAutoWake: (_) {},
            onAdvancedEffects: (_) {},
            onMiPlay: (_) {},
            onMiPlayIdentity: (_) {},
            onAirPlay: (_) {},
            onDlna: (_) {},
            onExportLogs: () async => exportCount++,
          ),
        ),
      ),
    );

    expect(find.text('关于应用'), findsOneWidget);
    expect(find.text('导出日志'), findsOneWidget);
    await tester.ensureVisible(find.byKey(const ValueKey('export-logs')));
    await tester.tap(find.byKey(const ValueKey('export-logs')));
    await tester.pump();

    expect(exportCount, 1);
  });

  testWidgets('select does not open settings while player control has focus', (
    tester,
  ) async {
    final controller = NativeAppController();
    addTearDown(controller.dispose);
    tester.view.physicalSize = const Size(1200, 600);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        home: FusionPlayHome(
          controller: controller,
          state: const AppState(
            initialized: true,
            playback: PlaybackSnapshot(
              title: 'Track',
              protocol: 'AirPlay',
              durationMs: 180000,
              streamActive: true,
            ),
          ),
        ),
      ),
    );
    await tester.pump();

    final playPause = FocusManager.instance.rootScope.descendants.singleWhere(
      (node) => node.debugLabel == 'player-play-pause',
    );
    playPause.requestFocus();
    await tester.pump();
    expect(playPause.hasPrimaryFocus, isTrue);

    await tester.sendKeyEvent(LogicalKeyboardKey.select);
    await tester.pumpAndSettle();

    final panel = tester.widget<AnimatedPositioned>(
      find.byType(AnimatedPositioned),
    );
    expect(panel.right, lessThan(0));
  });

  testWidgets('player focus follows settings progress and play pause space', (
    tester,
  ) async {
    final controller = NativeAppController();
    addTearDown(controller.dispose);
    tester.view.physicalSize = const Size(1200, 600);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        home: FusionPlayHome(
          controller: controller,
          state: const AppState(
            initialized: true,
            playback: PlaybackSnapshot(
              title: 'Track',
              protocol: 'AirPlay',
              durationMs: 180000,
              streamActive: true,
            ),
            remoteControl: RemoteControlState(
              available: true,
              commands: {'play_pause', 'seek'},
            ),
          ),
        ),
      ),
    );
    await tester.pump();

    final nodes = FocusManager.instance.rootScope.descendants;
    final progress = nodes.singleWhere(
      (node) => node.debugLabel == 'player-progress',
    );
    final playPause = nodes.singleWhere(
      (node) => node.debugLabel == 'player-play-pause',
    );
    final settings = nodes.singleWhere(
      (node) => node.debugLabel == 'settings-button',
    );

    progress.requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.pump();
    expect(playPause.hasPrimaryFocus, isTrue);

    progress.requestFocus();
    await tester.pump();
    await tester.sendKeyEvent(LogicalKeyboardKey.arrowUp);
    await tester.pump();
    expect(settings.hasPrimaryFocus, isTrue);

    await tester.sendKeyEvent(LogicalKeyboardKey.arrowDown);
    await tester.pump();
    expect(progress.hasPrimaryFocus, isTrue);

    final settingsRect = tester.getRect(find.byTooltip('设置'));
    expect(settingsRect.top, 14);
    expect(1200 - settingsRect.right, 14);
  });
}
