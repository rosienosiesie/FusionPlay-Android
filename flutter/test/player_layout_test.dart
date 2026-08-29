import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/models/app_state.dart';
import 'package:fusionplay_android_flutter/ui/player_layout.dart';
import 'package:fusionplay_android_flutter/ui/player_view.dart';

void main() {
  test('手机横屏使用对称且紧凑的播放器留白', () {
    final insets = playerViewportInsets(840, 360);

    expect(insets.horizontal, closeTo(23.4, .01));
    expect(insets.vertical, closeTo(16.2, .01));
  });

  test('窄横屏的封面和详情列不会超出可用宽度', () {
    final layout = playerLayoutMetrics(520, 260);

    expect(layout.horizontal, isTrue);
    expect(
      layout.artworkSize + layout.contentGap + layout.detailsMaxWidth,
      lessThanOrEqualTo(520),
    );
    expect(layout.controlScale, greaterThanOrEqualTo(.94));
  });

  test('标准横屏保持旧版参考尺寸', () {
    final layout = playerLayoutMetrics(1168, 608);

    expect(layout.horizontal, isTrue);
    expect(layout.artworkSize, 400);
    expect(layout.contentGap, 56);
    expect(layout.detailsMaxWidth, 430);
    expect(layout.textScale, 1);
  });

  test('外层横屏会覆盖安全区处理后的内部宽高误判', () {
    final layout = playerLayoutMetrics(427, 437, landscape: true);

    expect(layout.horizontal, isTrue);
    expect(
      layout.artworkSize + layout.contentGap + layout.detailsMaxWidth,
      lessThanOrEqualTo(427),
    );
  });

  testWidgets('近方形横屏仍使用横向播放器布局', (tester) async {
    tester.view.physicalSize = const Size(490, 480);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.resetPhysicalSize);
    addTearDown(tester.view.resetDevicePixelRatio);

    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: PlayerView(
            state: const AppState(initialized: true),
            artworkSource: null,
            artworkIdentity: null,
            onPlayback: (_) {},
            onSeek: (_) {},
            onVolume: (_) {},
          ),
        ),
      ),
    );

    expect(
      find.byKey(const ValueKey('player-layout-horizontal')),
      findsOneWidget,
    );
    expect(find.byKey(const ValueKey('player-layout-compact')), findsNothing);
  });
}
