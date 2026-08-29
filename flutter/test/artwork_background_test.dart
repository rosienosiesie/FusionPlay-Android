import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:fusionplay_android_flutter/ui/artwork.dart';

void main() {
  test('封面按上限尺寸解码以约束图片缓存占用', () {
    final artwork = File('test/fixtures/FusionPlay-Mark.png').absolute.path;

    expect(artworkProvider(artwork), isA<ResizeImage>());
  });

  testWidgets('背景只让已准备的新封面层淡入', (tester) async {
    final artwork = File('test/fixtures/FusionPlay-Mark.png').absolute.path;
    await tester.pumpWidget(
      MaterialApp(home: ArtworkBackground(source: artwork, hasPlayback: true)),
    );

    final artworkKey = ValueKey<String>('artwork-background:$artwork');
    const fallbackKey = ValueKey<String>('artwork-background:fallback:true');
    expect(find.byKey(artworkKey), findsOneWidget);
    expect(find.byKey(fallbackKey), findsNothing);

    await tester.pumpWidget(
      const MaterialApp(
        home: ArtworkBackground(source: null, hasPlayback: true),
      ),
    );
    expect(find.byKey(fallbackKey), findsOneWidget);
    expect(find.byKey(artworkKey), findsOneWidget);
  });

  testWidgets('关闭高级效果后封面背景不使用模糊', (tester) async {
    final artwork = File('test/fixtures/FusionPlay-Mark.png').absolute.path;
    await tester.pumpWidget(
      MaterialApp(
        home: MediaQuery(
          data: const MediaQueryData(disableAnimations: true),
          child: ArtworkBackground(source: artwork, hasPlayback: true),
        ),
      ),
    );

    expect(find.byType(ImageFiltered), findsNothing);
    expect(find.byType(Image), findsOneWidget);
  });

  test('封面翻转帧和时序参数与文档一致', () {
    final start = artworkFlipFrame(0, reducedMotion: false);
    final outgoingEdge = artworkFlipFrame(.499, reducedMotion: false);
    final incomingEdge = artworkFlipFrame(.5, reducedMotion: false);
    final end = artworkFlipFrame(1, reducedMotion: false);

    expect(ArtworkCard.flipDuration, const Duration(milliseconds: 240));
    expect(
      ArtworkCard.reducedMotionDuration,
      const Duration(milliseconds: 160),
    );
    expect(
      ArtworkBackground.transitionDuration,
      const Duration(milliseconds: 260),
    );
    expect(artworkClearGracePeriod, const Duration(milliseconds: 1200));
    expect(start.showIncoming, isFalse);
    expect(start.rotationY, 0);
    expect(start.opacity, 1);
    expect(outgoingEdge.rotationY, closeTo(1.567, .01));
    expect(incomingEdge.showIncoming, isTrue);
    expect(incomingEdge.rotationY, closeTo(-1.571, .01));
    expect(end.rotationY, 0);
    expect(end.opacity, 1);
  });

  test('减少动态效果只执行双阶段透明度过渡', () {
    final outgoing = artworkFlipFrame(.25, reducedMotion: true);
    final incoming = artworkFlipFrame(.75, reducedMotion: true);

    expect(outgoing.rotationY, 0);
    expect(incoming.rotationY, 0);
    expect(outgoing.opacity, .5);
    expect(incoming.opacity, .5);
  });

  test('上一曲向右翻转而下一曲向左翻转', () {
    final previous = artworkFlipFrame(
      .25,
      reducedMotion: false,
      direction: ArtworkFlipDirection.right,
    );
    final next = artworkFlipFrame(
      .25,
      reducedMotion: false,
      direction: ArtworkFlipDirection.left,
    );

    expect(previous.rotationY, greaterThan(0));
    expect(next.rotationY, lessThan(0));
    expect(previous.rotationY.abs(), closeTo(next.rotationY.abs(), .0001));
  });
}
